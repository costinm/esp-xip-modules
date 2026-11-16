//! Device control of the sys component (methods 5–8 and 10–11): module lifecycle, status, reset, saved settings and one debug packet send.
//! No text parser or renderer participates; the requests and answers are CBOR maps.
use alloc::{vec, vec::Vec};
use xip_abi::cbor::{self, Item, Reader, Writer};
use xip_abi::{Hw, ERR_SIZE, ERR_UNSUPPORTED};
use crate::RESP_MAX;

pub(crate) const M_STATUS: i32 = 5;
pub(crate) const M_START: i32 = 6;
pub(crate) const M_STOP: i32 = 7;
pub(crate) const M_RESET: i32 = 8;
pub(crate) const M_NV: i32 = 10;
pub(crate) const M_LINK_SEND: i32 = 11;
pub(crate) const M_QUARANTINE_CLEAR: i32 = 12;

/// Is `method` one of the device-control methods (their direct calls answer an error as a signed scalar)?
pub(crate) fn is_device_method(method: i32) -> bool {
    matches!(method, M_STATUS | M_START | M_STOP | M_RESET | M_NV | M_LINK_SEND | M_QUARANTINE_CLEAR)
}

fn binary_result(rc: i32) -> Result<Vec<u8>, i32> {
    if rc < 0 { Err(rc) } else { Ok(vec![0xa0]) }
}

pub(crate) fn valid_map(data: &[u8]) -> bool {
    let mut reader = Reader::new(data);
    if !matches!(reader.next(), Some(Item::Map(_))) { return false; }
    let mut check = Reader::new(data);
    check.skip() && check.rest().is_empty()
}

/// The device-control operations (CBOR request and answer).
pub(crate) fn binary_method(hw: &Hw, method: i32, data: &[u8]) -> Result<Vec<u8>, i32> {
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
        M_QUARANTINE_CLEAR => {
            let name = cbor::get_text(data, 1).map_err(|_| xip_abi::ERR_ARG)?
                .filter(|n| !n.is_empty() && n.len() < 16 && !n.contains(&0))
                .ok_or(xip_abi::ERR_ARG)?;
            let service = hw.get_component(c"module-quarantine").cast::<xip_abi::ModuleQuarantine>();
            if service.is_null() { return Err(ERR_UNSUPPORTED); }
            // The header is checked before reading the optional function pointer.
            unsafe {
                if (*service).version != 1 || ((*service).size as usize) < core::mem::size_of::<xip_abi::ModuleQuarantine>() {
                    return Err(ERR_UNSUPPORTED);
                }
                binary_result(((*service).clear)(hw.ctx(), name.as_ptr(), name.len()))
            }
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


#[cfg(test)]
mod quarantine_tests {
    use super::*;
    use core::ffi::{c_char, c_void};
    use xip_abi::{Host, ModuleQuarantine, ABI_VERSION, ERR_ARG};

    unsafe extern "C" fn component(user: *mut c_void, name: *const c_char) -> *const c_void {
        assert_eq!(unsafe { core::ffi::CStr::from_ptr(name) }, c"module-quarantine");
        user.cast_const()
    }
    unsafe extern "C" fn clear(_: *mut c_void, name: *const u8, len: usize) -> i32 {
        assert_eq!(unsafe { core::slice::from_raw_parts(name, len) }, b"nan");
        0
    }
    #[test]
    fn clear_validates_requests_and_optional_runtime_service() {
        // Host contains scalars, raw pointers and optional function pointers.
        let mut host: Host = unsafe { core::mem::zeroed() };
        host.abi_version = ABI_VERSION;
        host.size = core::mem::size_of::<Host>() as u32;
        host.get_component = Some(component);
        let request = b"\xa1\x01\x63nan";
        let run = |host: &Host, request: &[u8]| {
            binary_method(&unsafe { Hw::new(host).unwrap() }, M_QUARANTINE_CLEAR, request)
        };
        assert_eq!(run(&host, request), Err(ERR_UNSUPPORTED));
        assert_eq!(run(&host, b"\xa0"), Err(ERR_ARG));
        assert_eq!(run(&host, b"\xa1\x01\x60"), Err(ERR_ARG));
        assert_eq!(run(&host, b"\xa1\x01\x43nan"), Err(ERR_ARG));
        assert_eq!(run(&host, b"\xa1\x01\x63n\0n"), Err(ERR_ARG));
        let mut service = ModuleQuarantine { version: 1, size: core::mem::size_of::<ModuleQuarantine>() as u32, clear };
        host.user = core::ptr::addr_of_mut!(service).cast();
        assert_eq!(run(&host, request), Ok(vec![0xa0]));
        service.size = 8;
        host.user = core::ptr::addr_of_mut!(service).cast();
        assert_eq!(run(&host, request), Err(ERR_UNSUPPORTED));
        service.size = core::mem::size_of::<ModuleQuarantine>() as u32;
        service.version = 2;
        host.user = core::ptr::addr_of_mut!(service).cast();
        assert_eq!(run(&host, request), Err(ERR_UNSUPPORTED));
    }
}
