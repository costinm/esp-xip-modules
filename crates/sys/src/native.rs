//! The native method table of the sys component: the components that no module hosts (Wi-Fi 10, ip 12, ...) are answered from the
//! layouts of `tables.rs` (generated from `api/wifi.API.md` by `tools/catalog_rs.py`): the tagged fields of a request fill the struct,
//! the result is encoded from the struct the native function filled. Variable-length Wi-Fi answers are encoded here as well.
use alloc::{vec, vec::Vec};
use core::ffi::{c_int, c_void, CStr};
use xip_abi::cbor::{self, Item, Writer};
use xip_abi::{Hw, ERR_SIZE, ERR_UNSUPPORTED};
use crate::tables::*;
use crate::RESP_MAX;

pub(crate) fn nat_find(comp: u64, meth: u64) -> Option<&'static NatMethod> {
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

pub(crate) fn native_binary(hw: &Hw, nm: &NatMethod, fields: &[u8]) -> Result<Vec<u8>, i32> {
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


/// Variable-length Wi-Fi responses are encoded here, beside the generated
/// fixed-layout adapters. Driver lifecycle/scan ownership stays with the host.
pub(crate) fn wifi_binary(hw: &Hw, method: u64) -> Result<Vec<u8>, i32> {
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


#[cfg(test)]
mod decode_tests {
    use super::*;
    #[test]
    fn tuning_power_decodes_to_its_offset() {
        // the wire fields map: {12: 2} (power)
        let map = [0xa1, 0x0c, 0x02];
        let s = &NAT_STRUCTS[2]; // the wifi_tuning struct
        let mut b = [0u8; 24];
        assert!(nat_fill(s, &mut b, Some(&map)), "nat_fill accepts the power field");
        assert_eq!(b[11], 2, "power at offset 11");
        assert_eq!(&b[0..4], &24u32.to_le_bytes(), "the size field is filled");
        // and the get emits it back (uint key/value pairs; walk the raw bytes)
        let mut out = [0u8; 64]; let mut w = Writer::new(&mut out);
        let _pairs = nat_emit(s, &b, &mut w);
        let written = w.finish().unwrap();
        let mut i = 0; let mut decoded = Vec::new();
        while i + 1 < written.len() {
            decoded.push((written[i] as u64, written[i+1] as u64));
            i += 2;
        }
        assert!(decoded.contains(&(12, 2)), "power round-trips: {decoded:?}");
    }
}
