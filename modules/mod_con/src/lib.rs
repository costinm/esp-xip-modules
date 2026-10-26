//! CBOR device control and native-component dispatch. A request is a call (component, method, fields): the console's own methods (component 20), or, for
//! the runtime's native components without a hosting module (wifi 10, ip 12, ...), the native method table (`nat_find` / `native_binary`).
//! Requests reach it as 0x444d0002 datagrams parsed by the runtime (`call.is_envelope()`); this module has no port and parses no envelope of a port.
#![no_std]
extern crate alloc;
use alloc::{vec, vec::Vec};
use core::ffi::{c_int, c_void, CStr};
use xip_abi::cbor::{self, Item, Reader, Writer};
use xip_abi::{CallView, Hw, Module, ERR_SIZE, ERR_UNSUPPORTED};
mod tables;
use tables::*;

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

const C_CONSOLE: i32 = 20;
// IDs 1–3 belonged to the removed text interface and 4 to `dispatch` (an envelope as an argument, now the runtime's ADDR_ENVELOPE); do not reuse them.
const M_STATUS: i32 = 5;
const M_START: i32 = 6;
const M_STOP: i32 = 7;
const M_RESET: i32 = 8;
const M_NV: i32 = 10;
const M_LINK_SEND: i32 = 11;
const RESP_MAX: usize = 1000;

pub const STATE_BYTES: usize = 0;
#[derive(Default)]
pub struct State;

fn nat_find(comp: u64, meth: u64) -> Option<&'static NatMethod> {
    NAT_METHODS[..NAT_METHODS_N].iter().find(|m| m.comp as u64 == comp && m.method as u64 == meth)
}

fn load_le(p: &[u8]) -> u64 {
    p.iter().take(8).enumerate().fold(0u64, |a, (i, b)| a | (*b as u64) << (8 * i))
}

fn store_le(p: &mut [u8], v: u64) {
    for (i, b) in p.iter_mut().take(8).enumerate() {
        *b = (v >> (8 * i)) as u8;
    }
}

/// Does the CBOR integer (negative: -1 - arg) fit a field of `size` bytes?
fn nat_fits(size: usize, signed: bool, negative: bool, arg: u64) -> bool {
    if negative && !signed {
        return false;
    }
    if size >= 8 {
        return if signed { arg <= i64::MAX as u64 } else { true };
    }
    let limit: u64 = match (size, signed) {
        (1, true) => 0x7f,
        (1, false) => 0xff,
        (2, true) => 0x7fff,
        (2, false) => 0xffff,
        (_, true) => 0x7fff_ffff,
        (_, false) => 0xffff_ffff,
    };
    // for a negative value the CBOR argument is -1 - value, so the same limit holds (-128 is arg 127 of a signed byte)
    arg <= limit
}

fn hex_decode(digits: &[u8], out: &mut [u8]) -> Option<usize> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    if digits.len() % 2 != 0 || digits.len() / 2 > out.len() {
        return None;
    }
    for (slot, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *slot = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digits.len() / 2)
}

/// The struct bytes from the tagged fields of a request; an absent field stays zero.
fn nat_fill(s: &NatStruct, buf: &mut [u8], map: Option<&[u8]>) -> bool {
    for f in &NAT_FIELDS[s.first as usize..(s.first + s.count) as usize] {
        let off = f.offset as usize;
        let size = f.size as usize;
        if f.kind as u16 == NAT_K_SIZE {
            store_le(&mut buf[off..off + size], s.size as u64);
            continue;
        }
        let Some(map) = map else { continue };
        let Ok(Some(mut r)) = cbor::get(map, f.tag as u64) else { continue };
        let Some(it) = r.next() else { continue };
        if f.kind as u16 == NAT_K_TEXT || f.kind as u16 == NAT_K_BYTES {
            let (Item::Text(d) | Item::Bytes(d)) = it else {
                return false;
            };
            // DMesh clients (the lmesh catalog's `hex` kind) send a byte field as the text `hex:` and the digits.
            let mut hex = [0u8; 256];
            let d: &[u8] = match (it, f.kind as u16 == NAT_K_BYTES, d.strip_prefix(b"hex:")) {
                (Item::Text(_), true, Some(digits)) => match hex_decode(digits, &mut hex) {
                    Some(n) => &hex[..n],
                    None => return false,
                },
                _ => d,
            };
            if if f.len_size != 0 { d.len() > size } else { d.len() != size } {
                return false;
            }
            buf[off..off + d.len()].copy_from_slice(d);
            if f.len_size != 0 {
                let lo = f.len_offset as usize;
                store_le(&mut buf[lo..lo + f.len_size as usize], d.len() as u64);
            }
            continue;
        }
        if let (Item::Bool(b), true) = (it, f.kind as u16 == NAT_K_BOOL) {
            store_le(&mut buf[off..off + size], b as u64);
            continue;
        }
        let (negative, arg) = match it {
            Item::Uint(a) => (false, a),
            Item::Neg(a) => (true, a),
            _ => {
                return false;
            }
        };
        if !nat_fits(size, f.is_signed != 0, negative, arg) {
            return false;
        }
        let v = if negative { !arg } else { arg };
        store_le(&mut buf[off..off + size], v);
    }
    true
}

/// The tagged fields of a struct that has something to say (a text of length 0 or all-zero bytes are left out); the pairs go to `w`
/// without a map header.
fn nat_emit(s: &NatStruct, buf: &[u8], w: &mut Writer) -> usize {
    let mut pairs = 0;
    for f in &NAT_FIELDS[s.first as usize..(s.first + s.count) as usize] {
        let off = f.offset as usize;
        let size = f.size as usize;
        let p = &buf[off..off + size];
        let kind = f.kind as u16;
        if kind == NAT_K_SIZE {
            continue;
        }
        if kind == NAT_K_TEXT || kind == NAT_K_BYTES {
            let n = if f.len_size != 0 {
                let lo = f.len_offset as usize;
                (load_le(&buf[lo..lo + f.len_size as usize]) as usize).min(size)
            } else if p.iter().all(|b| *b == 0) {
                0
            } else {
                size
            };
            if n == 0 {
                continue;
            }
            w.uint(f.tag as u64);
            if kind == NAT_K_TEXT {
                w.text_raw(&p[..n]);
            } else {
                w.bytes(&p[..n]);
            }
        } else {
            let v = load_le(p);
            w.uint(f.tag as u64);
            if kind == NAT_K_BOOL {
                w.boolean(v != 0);
            } else if kind == NAT_K_INT {
                let bits = 8 * size.min(8) as u32;
                let signed = if bits >= 64 { v as i64 } else { ((v << (64 - bits)) as i64) >> (64 - bits) };
                w.int(signed);
            } else {
                w.uint(v);
            }
        }
        pairs += 1;
    }
    pairs
}

fn native_binary(hw: &Hw, nm: &NatMethod, fields: &[u8]) -> Result<Vec<u8>, i32> {
    let name = &NAT_POOL[nm.table as usize..];
    let Ok(cname_) = CStr::from_bytes_until_nul(name) else { return Err(xip_abi::ERR_ARG) };
    let table = hw.get_component(cname_) as *const u8;
    if table.is_null() {
        return Err(xip_abi::ERR_ARG);
    }
    let table_size = unsafe { core::ptr::read_unaligned(table.add(4) as *const u32) } as usize;
    let member = 8 + nm.slot as usize * core::mem::size_of::<usize>(); // after {version, size}
    if member + core::mem::size_of::<usize>() > table_size {
        return Err(xip_abi::ERR_ARG);
    }
    let fp = unsafe { core::ptr::read_unaligned(table.add(member) as *const usize) };
    if fp == 0 || nm.nparams > 4 {
        return Err(xip_abi::ERR_ARG);
    }
    let np = nm.nparams as usize;
    // each argument is a zeroed struct (4-byte aligned) or an 8-byte scalar slot
    let mut store: Vec<Vec<u32>> = Vec::new();
    let mut arg = [0usize; 4];
    let mut outs: [Option<usize>; 4] = [None; 4];
    for i in 0..np {
        let p = &NAT_PARAMS[nm.first_param as usize + i];
        if p.role == NAT_CTX {
            arg[i] = hw.ctx() as usize;
            continue;
        }
        let is_struct = p.what >= 0x100;
        let s = if is_struct { Some(&NAT_STRUCTS[p.what as usize - 0x100]) } else { None };
        let size = s.map_or(8, |s| s.size as usize);
        let mut b = vec![0u32; size.div_ceil(4)];
        if let Some(s) = s {
            let bytes = unsafe { core::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u8, size) };
            if !nat_fill(s, bytes, if p.role == NAT_IN { Some(fields) } else { None }) {
                return Err(xip_abi::ERR_ARG);
            }
        }
        arg[i] = b.as_ptr() as usize;
        if p.role == NAT_OUT {
            outs[i] = Some(store.len());
        }
        store.push(b);
    }
    let rc: c_int = unsafe {
        match np {
            1 => core::mem::transmute::<usize, extern "C" fn(*mut c_void) -> c_int>(fp)(arg[0] as *mut c_void),
            2 => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void) -> c_int>(fp)(arg[0] as *mut c_void, arg[1] as *mut c_void),
            3 => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> c_int>(fp)(
                arg[0] as *mut c_void,
                arg[1] as *mut c_void,
                arg[2] as *mut c_void,
            ),
            _ => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> c_int>(fp)(
                arg[0] as *mut c_void,
                arg[1] as *mut c_void,
                arg[2] as *mut c_void,
                arg[3] as *mut c_void,
            ),
        }
    };
    if rc < 0 {
        return Err(rc);
    }
    let mut scratch = vec![0u8; RESP_MAX];
    let mut pw = Writer::new(&mut scratch);
    let mut pairs = 0;
    for i in 0..np {
        let p = &NAT_PARAMS[nm.first_param as usize + i];
        let Some(si) = outs[i] else { continue };
        let bytes = unsafe { core::slice::from_raw_parts(store[si].as_ptr() as *const u8, store[si].len() * 4) };
        if p.what >= 0x100 {
            pairs += nat_emit(&NAT_STRUCTS[p.what as usize - 0x100], bytes, &mut pw);
            continue;
        }
        if p.tag == 0 {
            continue;
        }
        let v = load_le(&bytes[..p.what as usize]);
        pw.uint(p.tag as u64);
        if p.is_signed != 0 && p.what < 8 {
            pw.int(v as u32 as i32 as i64);
        } else {
            pw.uint(v);
        }
        pairs += 1;
    }
    let Some(body) = pw.finish() else {
        return Err(xip_abi::ERR_ARG);
    };
    let mut result = vec![0u8; RESP_MAX + 16];
    let mut rw = Writer::new(&mut result);
    rw.map(pairs).raw(body);
    let Some(res) = rw.finish() else {
        return Err(xip_abi::ERR_ARG);
    };
    Ok(res.to_vec())
}

fn binary_result(rc: i32) -> Result<Vec<u8>, i32> {
    if rc < 0 { Err(rc) } else { Ok(vec![0xa0]) }
}

fn valid_map(data: &[u8]) -> bool {
    let mut reader = Reader::new(data);
    if !matches!(reader.next(), Some(Item::Map(_))) { return false; }
    let mut check = Reader::new(data);
    check.skip() && check.rest().is_empty()
}

/// The console's binary operations. No text parser or renderer participates.
fn binary_method(hw: &Hw, method: i32, data: &[u8]) -> Result<Vec<u8>, i32> {
    if !valid_map(data) { return Err(xip_abi::ERR_ARG); }
    match method {
        M_STATUS => {
            let mut out = vec![0; RESP_MAX];
            loop {
                match hw.device_info(&mut out) {
                    Err(ERR_SIZE) if out.len() < 16384 => out.resize(out.len() * 2, 0),
                    Err(rc) => return Err(rc),
                    Ok(n) => { out.truncate(n); return Ok(out); }
                }
            }
        }
        M_START | M_STOP => {
            let name = cbor::get_text(data, 1).map_err(|_| xip_abi::ERR_ARG)?.filter(|n| !n.is_empty()).ok_or(xip_abi::ERR_ARG)?;
            if method == M_STOP { return binary_result(hw.module_stop(name)); }
            let request = match cbor::get(data, 2).map_err(|_| xip_abi::ERR_ARG)? {
                None => &[][..],
                Some(mut r) => match r.next() { Some(Item::Bytes(b)) => b, _ => return Err(xip_abi::ERR_ARG) },
            };
            binary_result(hw.module_start(name, request))
        }
        M_RESET => binary_result(hw.reset()),
        M_NV => {
            let key = cbor::get_text(data, 1).map_err(|_| xip_abi::ERR_ARG)?.filter(|k| !k.is_empty()).ok_or(xip_abi::ERR_ARG)?;
            let value = match cbor::get(data, 2).map_err(|_| xip_abi::ERR_ARG)? {
                None => None,
                Some(mut r) => match r.next() { Some(Item::Bytes(b)) => Some(b), _ => return Err(xip_abi::ERR_ARG) },
            };
            let mut value_out = vec![0; RESP_MAX];
            let n = hw.nv(key, value, &mut value_out);
            if n < 0 { return Err(n); }
            if value.is_some() { return Ok(vec![0xa0]); }
            let mut out = vec![0; n as usize + 16];
            let mut w = Writer::new(&mut out);
            w.map(1).uint(1).bytes(&value_out[..n as usize]);
            let n = w.finish().ok_or(ERR_SIZE)?.len();
            out.truncate(n);
            Ok(out)
        }
        M_LINK_SEND => link_send(hw, data),
        _ => Err(ERR_UNSUPPORTED),
    }
}

/// DEBUG ONLY: `link.send`. One packet of `data` (CBOR bytes, or the text `hex:`...) through the normal send path of the link at address `link`
/// (as the runtime reports it, e.g. 0x40000001): a pool packet, owned by the link once accepted. A busy link (`ERR_BUSY`) keeps nothing of ours.
fn link_send(hw: &Hw, data: &[u8]) -> Result<Vec<u8>, i32> {
    let link = cbor::get_int(data, 1).map_err(|_| xip_abi::ERR_ARG)?.ok_or(xip_abi::ERR_ARG)?;
    let link = u32::try_from(link).map_err(|_| xip_abi::ERR_ARG)?;
    let token = cbor::get_int(data, 3).map_err(|_| xip_abi::ERR_ARG)?.unwrap_or(0) as u64;
    let payload: Vec<u8> = match cbor::get(data, 2).map_err(|_| xip_abi::ERR_ARG)?.and_then(|mut r| r.next()) {
        Some(Item::Bytes(b)) => b.to_vec(),
        Some(Item::Text(t)) => {
            let digits = t.strip_prefix(b"hex:").ok_or(xip_abi::ERR_ARG)?;
            if digits.len() % 2 != 0 { return Err(xip_abi::ERR_ARG); }
            let nib = |c: u8| match c { b'0'..=b'9' => Some(c - b'0'), b'a'..=b'f' => Some(c - b'a' + 10), b'A'..=b'F' => Some(c - b'A' + 10), _ => None };
            let mut v = Vec::with_capacity(digits.len() / 2);
            for pair in digits.chunks_exact(2) { v.push(nib(pair[0]).ok_or(xip_abi::ERR_ARG)? << 4 | nib(pair[1]).ok_or(xip_abi::ERR_ARG)?); }
            v
        }
        _ => return Err(xip_abi::ERR_ARG),
    };
    if payload.is_empty() { return Err(xip_abi::ERR_ARG); }
    let pkt = hw.pkt_alloc()?;
    match hw.pkt_buf(pkt) {
        Ok((window, room)) if payload.len() <= room => unsafe { core::ptr::copy_nonoverlapping(payload.as_ptr(), window, payload.len()) },
        Ok(_) => { hw.pkt_free(pkt); return Err(ERR_SIZE); }
        Err(e) => { hw.pkt_free(pkt); return Err(e); }
    }
    let rc = hw.send(pkt, payload.len(), link, 0, 0, token);
    if rc == xip_abi::ERR_BUSY { hw.pkt_free(pkt); }     // BUSY is the one failure that leaves the packet with the sender
    binary_result(rc)
}

/// Variable-length Wi-Fi responses are encoded here, beside the generated
/// fixed-layout adapters. Driver lifecycle/scan ownership stays with the host.
fn wifi_binary(hw: &Hw, method: u64) -> Result<Vec<u8>, i32> {
    let wifi = hw.wifi().ok_or(ERR_UNSUPPORTED)?;
    let mut out = vec![0u8; RESP_MAX];
    let mut w = Writer::new(&mut out);
    if method == 3 {
        let mut state = xip_abi::WifiState { size: core::mem::size_of::<xip_abi::WifiState>() as u32, ..Default::default() };
        let rc = unsafe { wifi.state.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &mut state) };
        if rc < 0 { return Err(rc); }
        let radio = state.radio != 0 || state.sta != 0 || state.ap != 0 || state.channel != 0;
        let ssid = &state.ssid[..(state.ssid_len as usize).min(state.ssid.len())];
        let ssid = core::str::from_utf8(ssid).ok().filter(|s| !s.is_empty());
        let joined = state.sta == 2;
        let ip = joined && state.ip4 != [0; 4];
        let pairs = 6 + (state.sta_mac != [0; 6]) as usize + (state.ap_mac != [0; 6]) as usize
            + (joined && ssid.is_some()) as usize + joined as usize + ip as usize;
        w.map(pairs).uint(1).uint(state.sta as u64).uint(5).boolean(state.ap != 0)
            .uint(6).uint(state.channel as u64).uint(10).boolean(radio)
            .uint(11).boolean(state.monitoring != 0).uint(12).boolean(state.now != 0);
        if state.sta_mac != [0; 6] { w.uint(8).bytes(&state.sta_mac); }
        if state.ap_mac != [0; 6] { w.uint(9).bytes(&state.ap_mac); }
        if joined {
            if let Some(ssid) = ssid { w.uint(2).text(ssid); }
            w.uint(4).int(state.rssi as i64);
            if ip {
                let mut text = [0u8; 15];
                let mut n = 0;
                for (i, octet) in state.ip4.iter().copied().enumerate() {
                    if i != 0 { text[n] = b'.'; n += 1; }
                    if octet >= 100 { text[n] = b'0' + octet / 100; n += 1; }
                    if octet >= 10 { text[n] = b'0' + (octet / 10) % 10; n += 1; }
                    text[n] = b'0' + octet % 10; n += 1;
                }
                w.uint(3).text_raw(&text[..n]);
            }
        }
    } else {
        let mut aps = vec![unsafe { core::mem::zeroed::<xip_abi::WifiAp>() }; 16];
        let n = unsafe { wifi.scan.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), aps.as_mut_ptr(), aps.len()) };
        if n < 0 { return Err(n); }
        if n as usize > aps.len() { return Err(ERR_SIZE); }
        w.map(1).uint(1).array(n as usize);
        for ap in &aps[..n as usize] {
            if ap.ssid_len as usize > ap.ssid.len() { return Err(xip_abi::ERR_ARG); }
            w.map(5).uint(1).bytes(&ap.ssid[..ap.ssid_len as usize]).uint(2).bytes(&ap.bssid)
                .uint(3).int(ap.rssi as i64).uint(4).uint(ap.channel as u64).uint(5).uint(ap.auth as u64);
        }
    }
    let n = w.finish().ok_or(ERR_SIZE)?.len();
    out.truncate(n);
    Ok(out)
}

fn ip_control(hw: &Hw, method: i64, fields: &[u8]) -> Result<Vec<u8>, i32> {
    let table = hw.get_component(c"ip").cast::<xip_abi::Ip>();
    if table.is_null() { return Err(ERR_UNSUPPORTED); }
    let ip = unsafe { &*table };
    if ip.version != xip_abi::IP_VERSION || ip.size < core::mem::size_of::<xip_abi::Ip>() as u32 { return Err(ERR_UNSUPPORTED); }
    let link = cbor::get_int(fields, 1).map_err(|_| xip_abi::ERR_ARG)?.ok_or(xip_abi::ERR_ARG)?;
    let link = u32::try_from(link).map_err(|_| xip_abi::ERR_ARG)?;
    let rc = if method == 2 {
        let up = cbor::get_int(fields, 2).map_err(|_| xip_abi::ERR_ARG)?.ok_or(xip_abi::ERR_ARG)?;
        if !(0..=1).contains(&up) { return Err(xip_abi::ERR_ARG); }
        unsafe { ip.set_up.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), link, up as u32) }
    } else { unsafe { ip.close.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), link) } };
    binary_result(rc)
}

/// Run request `(comp, method)` with the field map `fields`: the console methods, the Wi-Fi and IP controls and the native components of the
/// catalog. The runtime delivers a request that arrived as an envelope (port or datagram) as an ordinary call, so this is its only entry;
/// the answer it makes is wrapped by the runtime. Remote module dispatch is performed by the firmware's manifest registry; never
/// synchronously call another module from its shared worker.
fn dispatch_request(hw: &Hw, comp: i64, method: i64, fields: &[u8]) -> Result<Vec<u8>, i32> {
    if comp <= 0 || method <= 0 || !valid_map(fields) { return Err(xip_abi::ERR_ARG); }
    if comp == C_CONSOLE as i64 {
        return binary_method(hw, i32::try_from(method).map_err(|_| xip_abi::ERR_ARG)?, fields);
    }
    if comp == 10 && (method == 3 || method == 4) {
        return wifi_binary(hw, method as u64);
    }
    if comp == 12 && matches!(method, 2 | 3) { return ip_control(hw, method, fields); }
    if let Some(nm) = nat_find(comp as u64, method as u64) {
        return native_binary(hw, nm, fields);
    }
    Err(ERR_UNSUPPORTED)
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, c: &CallView) -> i32 {
        const CONTINUE: i32 = xip_abi::RET_CONTINUE as i32;
        if c.flags & xip_abi::CALL_LINK != 0 { return CONTINUE; }     // a raw frame: requests arrive as calls
        if c.is_envelope() {
            match dispatch_request(hw, c.component as i64, c.method as i64, c.data) {
                Ok(result) => { hw.reply(c, &result); }
                Err(rc) => { hw.reply_error(c, rc); }
            }
            return CONTINUE;
        }
        if c.component != C_CONSOLE || !c.wants_response() { return CONTINUE; }
        let out = match binary_method(hw, c.method, c.data) {
            Ok(result) => result,
            Err(rc) => {
                let mut out = vec![0; 9];
                let mut w = Writer::new(&mut out);
                w.int(rc as i64);
                let n = w.finish().map_or(0, |r| r.len());
                out.truncate(n);
                out
            }
        };
        if !out.is_empty() {
            hw.reply(c, &out);
        }
        CONTINUE
    }
}

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(feature = "sim")]
export!();
