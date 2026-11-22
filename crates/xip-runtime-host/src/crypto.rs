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
unsafe extern "C" fn aead_seal(user: *mut c_void, alg: u32, pkt: u32, offset: u32, key_len: u32, aad_len: u32, text_len: u32) -> i32 {
    let (sh, _) = ctx(user);
    if alg != xip_psa::ALG_GCM && alg != xip_psa::ALG_CCM {
        return ERR_UNSUPPORTED;
    }
    if key_len != 16 && key_len != 32 {
        return ERR_ARG;
    }
    let Some(hdr) = (key_len as u64).checked_add(12).and_then(|v| v.checked_add(aad_len as u64)) else { return ERR_ARG };
    let Some(total) = hdr.checked_add(text_len as u64).and_then(|v| v.checked_add(16)) else { return ERR_ARG };
    let Ok(total) = u32::try_from(total) else { return ERR_ARG };
    let all = match range(sh, pkt, offset, total, true) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let (head, rest) = all.split_at_mut(hdr as usize);
    let (key, nonce_aad) = head.split_at(key_len as usize);
    let (nonce, aad) = nonce_aad.split_at(12);
    let nonce: &[u8; 12] = nonce.try_into().unwrap();
    let (body, tag_out) = rest.split_at_mut(text_len as usize);
    let Some(tag) = xip_psa::aead_seal_in_place(alg, key, nonce, aad, body) else {
        return ERR_ARG;
    };
    tag_out.copy_from_slice(&tag);
    (text_len + 16) as i32
}
unsafe extern "C" fn aead_open(user: *mut c_void, alg: u32, pkt: u32, offset: u32, key_len: u32, aad_len: u32, text_len: u32) -> i32 {
    let (sh, _) = ctx(user);
    if alg != xip_psa::ALG_GCM && alg != xip_psa::ALG_CCM {
        return ERR_UNSUPPORTED;
    }
    if (key_len != 16 && key_len != 32) || text_len < 16 {
        return ERR_ARG;
    }
    let Some(hdr) = (key_len as u64).checked_add(12).and_then(|v| v.checked_add(aad_len as u64)) else { return ERR_ARG };
    let Some(total) = hdr.checked_add(text_len as u64) else { return ERR_ARG };
    let Ok(total) = u32::try_from(total) else { return ERR_ARG };
    let all = match range(sh, pkt, offset, total, true) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let (head, rest) = all.split_at_mut(hdr as usize);
    let (key, nonce_aad) = head.split_at(key_len as usize);
    let (nonce, aad) = nonce_aad.split_at(12);
    let nonce: &[u8; 12] = nonce.try_into().unwrap();
    let plain_len = (text_len - 16) as usize;
    let (body, tag_in) = rest.split_at_mut(plain_len);
    let tag: [u8; 16] = tag_in.try_into().unwrap();
    if xip_psa::aead_open_in_place(alg, key, nonce, aad, body, &tag) {
        plain_len as i32
    } else {
        ERR_AUTH
    }
}

unsafe extern "C" fn aead_pkt(
    user: *mut c_void,
    spec: u32,
    src_pkt: u32,
    src_off: u32,
    dst_pkt: u32,
    dst_off: u32,
    text_len: u32,
) -> i32 {
    let (sh, _) = ctx(user);
    let aad_len = (spec & 0xff) as usize;
    let key_len = ((spec >> 8) & 0xff) as usize;
    let alg = match (spec >> 16) & 0xff {
        0 => xip_psa::ALG_GCM,
        1 => xip_psa::ALG_CCM,
        _ => return ERR_UNSUPPORTED,
    };
    let is_open = (spec & AEAD_PKT_FLAG_OPEN) != 0;
    if (key_len != 16 && key_len != 32) || (is_open && text_len < 16) {
        return ERR_ARG;
    }
    let Some(src_e) = sh.pool.get(src_pkt) else { return ERR_ARG };
    let Some(dst_e) = sh.pool.get(dst_pkt) else { return ERR_ARG };
    if src_e.slot == dst_e.slot {
        return ERR_ARG;
    }
    if sh.pool.refs(dst_e.slot as i32) != 1 {
        return ERR_BUSY;
    }
    let slot_cap = sh.pool.slot_size() as usize;
    let hdr_len = key_len + 12 + aad_len;
    if (dst_off as usize) < hdr_len {
        return ERR_ARG;
    }
    let out_len = if is_open { (text_len - 16) as usize } else { text_len as usize + 16 };
    let Some(src_end) = (src_off as usize).checked_add(text_len as usize) else { return ERR_ARG };
    let Some(dst_end) = (dst_off as usize).checked_add(out_len) else { return ERR_ARG };
    if src_end > slot_cap || dst_end > slot_cap {
        return ERR_ARG;
    }
    let src_ptr = sh.pool.slot(src_e.slot as i32);
    let dst_ptr = sh.pool.slot(dst_e.slot as i32);
    if src_ptr.is_null() || dst_ptr.is_null() {
        return ERR_ARG;
    }
    let src_slot = std::slice::from_raw_parts(src_ptr, slot_cap);
    let dst_slot = std::slice::from_raw_parts_mut(dst_ptr, slot_cap);

    let mut key_buf = [0u8; 32];
    let mut nonce_buf = [0u8; 12];
    let mut aad_buf = [0u8; 255];
    key_buf[..key_len].copy_from_slice(&dst_slot[..key_len]);
    nonce_buf.copy_from_slice(&dst_slot[key_len..key_len + 12]);
    if aad_len > 0 {
        aad_buf[..aad_len].copy_from_slice(&dst_slot[key_len + 12..hdr_len]);
    }
    dst_slot[..hdr_len].fill(0);

    let res = if !is_open {
        let plain_len = text_len as usize;
        if plain_len > 0 {
            dst_slot[dst_off as usize..dst_off as usize + plain_len]
                .copy_from_slice(&src_slot[src_off as usize..src_off as usize + plain_len]);
        }
        let body = &mut dst_slot[dst_off as usize..dst_off as usize + plain_len];
        match xip_psa::aead_seal_in_place(alg, &key_buf[..key_len], &nonce_buf, &aad_buf[..aad_len], body) {
            Some(tag) => {
                dst_slot[dst_off as usize + plain_len..dst_end].copy_from_slice(&tag);
                out_len as i32
            }
            None => ERR_ARG,
        }
    } else {
        let plain_len = out_len;
        if plain_len > 0 {
            dst_slot[dst_off as usize..dst_off as usize + plain_len]
                .copy_from_slice(&src_slot[src_off as usize..src_off as usize + plain_len]);
        }
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&src_slot[src_off as usize + plain_len..src_end]);
        let body = &mut dst_slot[dst_off as usize..dst_end];
        if xip_psa::aead_open_in_place(alg, &key_buf[..key_len], &nonce_buf, &aad_buf[..aad_len], body, &tag) {
            plain_len as i32
        } else {
            ERR_AUTH
        }
    };
    key_buf.fill(0);
    nonce_buf.fill(0);
    res
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
    aead_seal: Some(aead_seal),
    aead_open: Some(aead_open),
    aead_pkt: Some(aead_pkt),
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
