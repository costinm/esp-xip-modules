//! The `crypto` and `identity` tables of the runtime: byte ranges of pool packets in, results written into packets. The cryptography is
//! `xip-psa` (pure Rust); this file only checks handles and ranges. The C runtime answers the same calls with `psa_*`.
use crate::hostcalls::ctx;
use std::ffi::c_void;
use xip_abi::*;

/// The `len` bytes at `offset` of the payload window of `pkt`. Reading needs a live handle; `write` also needs the packet to be unshared
/// (as `pkt_buf` does).
unsafe fn range<'a>(sh: &crate::runtime::Shared, pkt: u32, offset: u32, len: u32, write: bool) -> Result<&'a mut [u8], i32> {
    let e = sh.pool.get(pkt).ok_or(ERR_ARG)?;
    if write && sh.pool.refs(e.slot as i32) != 1 {
        return Err(ERR_BUSY);
    }
    let end = (offset as u64).checked_add(len as u64).ok_or(ERR_ARG)?;
    if end > PKT_MAX as u64 {
        return Err(ERR_ARG);
    }
    Ok(std::slice::from_raw_parts_mut(sh.pool.slot(e.slot as i32).add(e.start as usize).add(offset as usize), len as usize))
}
unsafe fn put(sh: &crate::runtime::Shared, pkt: u32, offset: u32, data: &[u8]) -> i32 {
    match range(sh, pkt, offset, data.len() as u32, true) {
        Ok(out) => {
            out.copy_from_slice(data);
            data.len() as i32
        }
        Err(e) => e,
    }
}

unsafe extern "C" fn hash(user: *mut c_void, alg: u32, pkt: u32, offset: u32, len: u32, out_pkt: u32, out_offset: u32) -> i32 {
    let (sh, _) = ctx(user);
    let input = match range(sh, pkt, offset, len, false) {
        Ok(i) => i,
        Err(e) => return e,
    };
    match xip_psa::hash(alg, input) {
        Some(d) => put(sh, out_pkt, out_offset, &d),
        None => ERR_UNSUPPORTED,
    }
}
unsafe extern "C" fn hash_start(user: *mut c_void, alg: u32) -> i32 {
    let (sh, me) = ctx(user);
    let Some(h) = xip_psa::Hasher::new(alg) else { return ERR_UNSUPPORTED };
    let mut hashes = sh.hashes.borrow_mut();
    if hashes.values().filter(|(o, _)| *o == me.id).count() >= 8 {
        return ERR_BUSY;
    }
    let mut handle = 1 + hashes.len() as u32;
    while hashes.contains_key(&handle) {
        handle += 1;
    }
    hashes.insert(handle, (me.id, h));
    handle as i32
}
unsafe extern "C" fn hash_update(user: *mut c_void, handle: u32, pkt: u32, offset: u32, len: u32) -> i32 {
    let (sh, me) = ctx(user);
    let input = match range(sh, pkt, offset, len, false) {
        Ok(i) => i,
        Err(e) => return e,
    };
    match sh.hashes.borrow_mut().get_mut(&handle) {
        Some((owner, h)) if *owner == me.id => {
            h.update(input);
            0
        }
        _ => ERR_NOTFOUND,
    }
}
unsafe extern "C" fn hash_finish(user: *mut c_void, handle: u32, out_pkt: u32, out_offset: u32) -> i32 {
    let (sh, me) = ctx(user);
    let owned = sh.hashes.borrow().get(&handle).is_some_and(|(o, _)| *o == me.id);
    if !owned {
        return ERR_NOTFOUND;
    }
    let (_, h) = sh.hashes.borrow_mut().remove(&handle).unwrap();
    put(sh, out_pkt, out_offset, &h.finish())
}
unsafe extern "C" fn hash_abort(user: *mut c_void, handle: u32) -> i32 {
    let (sh, me) = ctx(user);
    let mut hashes = sh.hashes.borrow_mut();
    if hashes.get(&handle).is_some_and(|(o, _)| *o == me.id) {
        hashes.remove(&handle);
    }
    0
}
unsafe extern "C" fn verify(user: *mut c_void, alg: u32, pkt: u32, offset: u32, key_len: u32, message_len: u32, signature_len: u32) -> i32 {
    let (sh, _) = ctx(user);
    if alg != xip_psa::ALG_ECDSA_SHA_256 {
        return ERR_UNSUPPORTED;
    }
    let total = key_len as u64 + message_len as u64 + signature_len as u64;
    let Ok(total) = u32::try_from(total) else { return ERR_ARG };
    let all = match range(sh, pkt, offset, total, false) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let (key, rest) = all.split_at(key_len as usize);
    let (message, signature) = rest.split_at(message_len as usize);
    if xip_psa::verify_p256_sha256(key, message, signature) { 0 } else { ERR_AUTH }
}
unsafe extern "C" fn random(user: *mut c_void, pkt: u32, offset: u32, len: u32) -> i32 {
    let (sh, _) = ctx(user);
    match range(sh, pkt, offset, len, true) {
        Ok(out) => {
            xip_psa::random(out);
            0
        }
        Err(e) => e,
    }
}

static CRYPTO: Crypto = Crypto {
    version: CRYPTO_VERSION,
    size: std::mem::size_of::<Crypto>() as u32,
    hash: Some(hash),
    hash_start: Some(hash_start),
    hash_update: Some(hash_update),
    hash_finish: Some(hash_finish),
    hash_abort: Some(hash_abort),
    verify: Some(verify),
    random: Some(random),
};
pub(crate) fn crypto_component() -> *const Crypto {
    &CRYPTO
}

unsafe extern "C" fn identity_public_key(user: *mut c_void, pkt: u32, offset: u32) -> i32 {
    let (sh, _) = ctx(user);
    let r = put(sh, pkt, offset, &xip_psa::identity_public_key());
    if r < 0 { r } else { 0 }
}
unsafe extern "C" fn identity_sign(user: *mut c_void, pkt: u32, offset: u32, len: u32, out_pkt: u32, out_offset: u32) -> i32 {
    let (sh, _) = ctx(user);
    let message = match range(sh, pkt, offset, len, false) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let signature = xip_psa::identity_sign(message);
    let r = put(sh, out_pkt, out_offset, &signature);
    if r < 0 { r } else { 0 }
}
static IDENTITY: Identity = Identity {
    version: IDENTITY_VERSION,
    size: std::mem::size_of::<Identity>() as u32,
    public_key: Some(identity_public_key),
    sign: Some(identity_sign),
};
pub(crate) fn identity_component() -> *const Identity {
    &IDENTITY
}
