//! Test provider of the runtime's `crypto` table: the same table and client as the firmware, over a fake packet pool
//! (`pkts`); the cryptography is `xip-psa`, so the tests exercise the real code paths of the module.
use crate::crypto::Psa;
use core::{
    ffi::{c_char, c_void, CStr},
    mem,
};
use std::{
    boxed::Box,
    cell::{Cell, RefCell},
    collections::HashMap,
    thread_local,
};
use xip_abi::{Crypto, CRYPTO_VERSION, ERR_ARG, ERR_AUTH, ERR_NOTFOUND, ERR_UNSUPPORTED};

/// The fake packet pool of the tests: handle to bytes (a payload window of 1500 bytes).
pub(super) mod pkts {
    use core::cell::{Cell, RefCell};
    use std::{collections::HashMap, thread_local, vec, vec::Vec};
    use xip_abi::ERR_ARG;
    thread_local! {
        static PACKETS: RefCell<HashMap<u32, Vec<u8>>> = RefCell::new(HashMap::new());
        static NEXT: Cell<u32> = const { Cell::new(1) };
    }
    pub fn alloc() -> u32 {
        let id = NEXT.with(|n| n.replace(n.get() + 1));
        PACKETS.with(|p| p.borrow_mut().insert(id, vec![0; 1500]));
        id
    }
    pub fn remove(id: u32) -> Option<Vec<u8>> {
        PACKETS.with(|p| p.borrow_mut().remove(&id))
    }
    pub fn is_empty() -> bool {
        PACKETS.with(|p| p.borrow().is_empty())
    }
    pub unsafe fn window(id: u32, out: *mut *mut u8, cap: *mut usize) -> i32 {
        PACKETS.with(|p| match p.borrow_mut().get_mut(&id) {
            Some(b) => {
                *out = b.as_mut_ptr();
                *cap = b.len();
                0
            }
            None => ERR_ARG,
        })
    }
    pub fn with<T>(id: u32, f: impl FnOnce(&mut [u8]) -> T) -> Option<T> {
        PACKETS.with(|p| p.borrow_mut().get_mut(&id).map(|b| f(b)))
    }
}

thread_local! {
    static HASHES: RefCell<HashMap<u32, xip_psa::Hasher>> = RefCell::new(HashMap::new());
    static NEXT_HASH: Cell<u32> = const { Cell::new(1) };
    static FAIL_START: Cell<bool> = const { Cell::new(false) };
}

fn bytes(pkt: u32, offset: u32, len: u32) -> Option<std::vec::Vec<u8>> {
    pkts::with(pkt, |b| b.get(offset as usize..(offset + len) as usize).map(|s| s.to_vec())).flatten()
}
fn put(pkt: u32, offset: u32, data: &[u8]) -> i32 {
    pkts::with(pkt, |b| match b.get_mut(offset as usize..offset as usize + data.len()) {
        Some(d) => {
            d.copy_from_slice(data);
            data.len() as i32
        }
        None => ERR_ARG,
    })
    .unwrap_or(ERR_ARG)
}
unsafe extern "C" fn hash(_: *mut c_void, alg: u32, pkt: u32, offset: u32, len: u32, out: u32, out_offset: u32) -> i32 {
    match (bytes(pkt, offset, len), xip_psa::hash(alg, &bytes(pkt, offset, len).unwrap_or_default())) {
        (Some(_), Some(d)) => put(out, out_offset, &d),
        _ => ERR_ARG,
    }
}
unsafe extern "C" fn hash_start(_: *mut c_void, alg: u32) -> i32 {
    if FAIL_START.with(|f| f.replace(false)) {
        return xip_abi::ERR_NOMEM;
    }
    let Some(h) = xip_psa::Hasher::new(alg) else { return ERR_UNSUPPORTED };
    let id = NEXT_HASH.with(|n| n.replace(n.get() + 1));
    HASHES.with(|h2| h2.borrow_mut().insert(id, h));
    id as i32
}
unsafe extern "C" fn hash_update(_: *mut c_void, handle: u32, pkt: u32, offset: u32, len: u32) -> i32 {
    let Some(data) = bytes(pkt, offset, len) else { return ERR_ARG };
    HASHES.with(|h| match h.borrow_mut().get_mut(&handle) {
        Some(h) => {
            h.update(&data);
            0
        }
        None => ERR_NOTFOUND,
    })
}
unsafe extern "C" fn hash_finish(_: *mut c_void, handle: u32, out: u32, out_offset: u32) -> i32 {
    match HASHES.with(|h| h.borrow_mut().remove(&handle)) {
        Some(h) => put(out, out_offset, &h.finish()),
        None => ERR_NOTFOUND,
    }
}
unsafe extern "C" fn hash_abort(_: *mut c_void, handle: u32) -> i32 {
    HASHES.with(|h| h.borrow_mut().remove(&handle));
    0
}
unsafe extern "C" fn verify(_: *mut c_void, alg: u32, pkt: u32, offset: u32, key_len: u32, message_len: u32, signature_len: u32) -> i32 {
    if alg != xip_psa::ALG_ECDSA_SHA_256 {
        return ERR_UNSUPPORTED;
    }
    let Some(all) = bytes(pkt, offset, key_len + message_len + signature_len) else { return ERR_ARG };
    let (key, rest) = all.split_at(key_len as usize);
    let (message, signature) = rest.split_at(message_len as usize);
    if xip_psa::verify_p256_sha256(key, message, signature) { 0 } else { ERR_AUTH }
}
unsafe extern "C" fn random(_: *mut c_void, pkt: u32, offset: u32, len: u32) -> i32 {
    let mut data = std::vec![0u8; len as usize];
    xip_psa::random(&mut data);
    let r = put(pkt, offset, &data);
    if r < 0 { r } else { 0 }
}

static TABLE: Crypto = Crypto {
    version: CRYPTO_VERSION,
    size: mem::size_of::<Crypto>() as u32,
    hash: Some(hash),
    hash_start: Some(hash_start),
    hash_update: Some(hash_update),
    hash_finish: Some(hash_finish),
    hash_abort: Some(hash_abort),
    verify: Some(verify),
    random: Some(random),
};
pub(super) unsafe extern "C" fn get_component(_: *mut c_void, name: *const c_char) -> *const c_void {
    if !name.is_null() && CStr::from_ptr(name).to_bytes() == b"crypto" {
        (&TABLE as *const Crypto).cast()
    } else {
        core::ptr::null()
    }
}
unsafe extern "C" fn pkt_alloc(_: *mut c_void, p: *mut u32) -> i32 {
    *p = pkts::alloc();
    0
}
unsafe extern "C" fn pkt_buf(_: *mut c_void, p: u32, out: *mut *mut u8, cap: *mut usize) -> i32 {
    pkts::window(p, out, cap)
}
unsafe extern "C" fn pkt_free(_: *mut c_void, p: u32) -> i32 {
    pkts::remove(p);
    0
}
/// A client over a host with only the crypto table and the packet pool.
pub(super) fn psa() -> Psa {
    let mut host: Box<xip_abi::Host> = Box::new(unsafe { mem::zeroed() });
    host.abi_version = xip_abi::ABI_VERSION;
    host.size = mem::size_of::<xip_abi::Host>() as u32;
    host.get_component = Some(get_component);
    host.pkt_alloc = Some(pkt_alloc);
    host.pkt_buf = Some(pkt_buf);
    host.pkt_free = Some(pkt_free);
    let host: &'static xip_abi::Host = Box::leak(host);
    Psa::from_host(unsafe { crate::Hw::new(host) }.unwrap()).unwrap()
}
fn no_resources() {
    HASHES.with(|h| assert!(h.borrow().is_empty(), "hash operation leaked"));
    assert!(pkts::is_empty(), "packet leaked");
}
#[test]
fn table_hash_vectors_and_stream_drop() {
    let psa = psa();
    let abc = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17,
        0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
    ];
    assert_eq!(psa.sha256(b"abc").unwrap(), abc);
    let mut hash = psa.sha256_stream().unwrap();
    hash.update(b"a").unwrap();
    hash.update(b"bc").unwrap();
    assert_eq!(hash.finish().unwrap(), abc);
    drop(hash);
    let big = std::vec![0x5a; 5000]; // several packets
    let mut hash = psa.sha256_stream().unwrap();
    hash.update(&big).unwrap();
    assert_eq!(hash.finish().unwrap()[..], xip_psa::hash(xip_psa::ALG_SHA_256, &big).unwrap()[..]);
    let hash = psa.sha256_stream().unwrap();
    drop(hash); // Disconnect before finish.
    FAIL_START.with(|f| f.set(true));
    assert!(psa.sha256_stream().is_err());
    assert_eq!(psa.sha256(b"").unwrap()[..], xip_psa::hash(xip_psa::ALG_SHA_256, b"").unwrap()[..]);
    no_resources();
}
#[test]
fn table_verification_accepts_valid_and_rejects_changes() {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    let public_key: [u8; 65] = key.verifying_key().to_encoded_point(false).as_bytes().try_into().unwrap();
    let signature: Signature = key.sign(b"signed header");
    let signature: [u8; 64] = signature.to_bytes().into();
    let psa = psa();
    assert!(psa.verify_p256_sha256(&public_key, &signature, b"signed header").is_ok());
    assert_eq!(psa.verify_p256_sha256(&public_key, &signature, b"changed"), Err(ERR_AUTH));
    let mut bad = signature;
    bad[0] ^= 1;
    assert!(psa.verify_p256_sha256(&public_key, &bad, b"signed header").is_err());
    let mut bad = public_key;
    bad[0] = 0;
    assert!(psa.verify_p256_sha256(&bad, &signature, b"signed header").is_err());
    no_resources();
}
#[test]
fn lookup_rejects_unknown_names_old_hosts_and_short_tables() {
    let mut host: xip_abi::Host = unsafe { mem::zeroed() };
    host.abi_version = xip_abi::ABI_VERSION;
    host.size = mem::size_of_val(&host) as u32;
    host.get_component = Some(get_component);
    let hw = unsafe { crate::Hw::new(&host) }.unwrap();
    assert!(Psa::from_host(hw).is_some());
    assert!(hw.get_component(CStr::from_bytes_with_nul(b"unknown\0").unwrap()).is_null());
    host.size = mem::offset_of!(xip_abi::Host, get_component) as u32;
    let hw = unsafe { crate::Hw::new(&host) }.unwrap();
    assert!(Psa::from_host(hw).is_none());
}
