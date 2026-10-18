//! Native-signature test provider. Upload tests exercise the same table/wrapper
//! as firmware; software crypto exists only behind these test callbacks.
use crate::crypto::Psa;
use core::{
    ffi::{c_char, c_void, CStr},
    mem, slice,
};
use xip_abi::psa::*;
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use sha2::{Digest, Sha256 as SoftwareSha256};
use std::{
    boxed::Box,
    cell::{Cell, RefCell},
    collections::HashMap,
    thread_local,
};
thread_local! {
    static KEYS: RefCell<HashMap<u32, VerifyingKey>> = RefCell::new(HashMap::new());
    static NEXT_KEY: Cell<u32> = const { Cell::new(1) };
    static HASHES: Cell<usize> = const { Cell::new(0) };
    static FAIL_SETUP: Cell<bool> = const { Cell::new(false) };
}
unsafe extern "C" fn hash_compute(
    alg: u32,
    input: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    if alg != ALG_SHA_256 || cap < 32 {
        return ERROR_INVALID_ARGUMENT;
    }
    slice::from_raw_parts_mut(out, 32)
        .copy_from_slice(&SoftwareSha256::digest(slice::from_raw_parts(input, len)));
    *written = 32;
    0
}
unsafe extern "C" fn hash_setup(op: *mut HashOperation, alg: u32) -> i32 {
    if alg != ALG_SHA_256 || (*op).id != 0 {
        return ERROR_BAD_STATE;
    }
    (*op).ctx.esp_ctx = EspHashContext {
        sha_ctx: Box::into_raw(Box::new(SoftwareSha256::new())).cast(),
        sha_type: 0,
    };
    (*op).id = 1;
    HASHES.with(|n| n.set(n.get() + 1));
    if FAIL_SETUP.with(|f| f.replace(false)) {
        return ERROR_INSUFFICIENT_MEMORY;
    }
    0
}
unsafe extern "C" fn hash_update(op: *mut HashOperation, input: *const u8, len: usize) -> i32 {
    if (*op).id != 1 {
        return ERROR_BAD_STATE;
    }
    let hash = &mut *(*op).ctx.esp_ctx.sha_ctx.cast::<SoftwareSha256>();
    hash.update(slice::from_raw_parts(input, len));
    0
}
unsafe extern "C" fn hash_finish(
    op: *mut HashOperation,
    out: *mut u8,
    cap: usize,
    written: *mut usize,
) -> i32 {
    if (*op).id != 1 || cap < 32 {
        return ERROR_BAD_STATE;
    }
    let hash = Box::from_raw((*op).ctx.esp_ctx.sha_ctx.cast::<SoftwareSha256>());
    (*op).id = 0;
    HASHES.with(|n| n.set(n.get() - 1));
    slice::from_raw_parts_mut(out, 32).copy_from_slice(&hash.finalize());
    *written = 32;
    0
}
unsafe extern "C" fn hash_abort(op: *mut HashOperation) -> i32 {
    if (*op).id != 0 {
        drop(Box::from_raw(
            (*op).ctx.esp_ctx.sha_ctx.cast::<SoftwareSha256>(),
        ));
        (*op).id = 0;
        HASHES.with(|n| n.set(n.get() - 1));
    }
    0
}
unsafe extern "C" fn import_key(
    attr: *const KeyAttributes,
    input: *const u8,
    len: usize,
    out: *mut u32,
) -> i32 {
    let attr = &*attr;
    if attr.key_type != KEY_TYPE_ECC_PUBLIC_KEY_SECP_R1
        || attr.bits != 256
        || attr.policy.alg != ALG_ECDSA_SHA_256
        || attr.policy.usage & KEY_USAGE_VERIFY_HASH == 0
    {
        return ERROR_INVALID_ARGUMENT;
    }
    let Ok(key) = VerifyingKey::from_sec1_bytes(slice::from_raw_parts(input, len)) else {
        return ERROR_INVALID_ARGUMENT;
    };
    *out = NEXT_KEY.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    KEYS.with(|keys| {
        keys.borrow_mut().insert(*out, key);
    });
    0
}
unsafe extern "C" fn destroy_key(key: u32) -> i32 {
    if KEYS.with(|keys| keys.borrow_mut().remove(&key).is_some()) {
        0
    } else {
        ERROR_INVALID_ARGUMENT
    }
}
unsafe extern "C" fn verify_hash(
    key: u32,
    alg: u32,
    hash: *const u8,
    len: usize,
    sig: *const u8,
    siglen: usize,
) -> i32 {
    if alg != ALG_ECDSA_SHA_256 || len != 32 {
        return ERROR_INVALID_ARGUMENT;
    }
    let Ok(sig) = Signature::from_slice(slice::from_raw_parts(sig, siglen)) else {
        return ERROR_INVALID_SIGNATURE;
    };
    let valid = KEYS.with(|keys| {
        keys.borrow().get(&key).is_some_and(|k| {
            k.verify_prehash(slice::from_raw_parts(hash, len), &sig)
                .is_ok()
        })
    });
    if valid {
        0
    } else {
        ERROR_INVALID_SIGNATURE
    }
}
unsafe extern "C" fn unsupported_random(_: *mut u8, _: usize) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_generate(_: *const KeyAttributes, _: *mut u32) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_export(_: u32, _: *mut u8, _: usize, _: *mut usize) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_agreement(
    _: u32,
    _: u32,
    _: *const u8,
    _: usize,
    _: *mut u8,
    _: usize,
    _: *mut usize,
) -> i32 {
    -134
}
unsafe extern "C" fn reset_attributes(a: *mut KeyAttributes) {
    *a = KeyAttributes::default();
}
unsafe extern "C" fn unsupported_cipher_setup(_: *mut CipherOperation, _: u32, _: u32) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_cipher_input(
    _: *mut CipherOperation,
    _: *const u8,
    _: usize,
) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_cipher_update(
    _: *mut CipherOperation,
    _: *const u8,
    _: usize,
    _: *mut u8,
    _: usize,
    _: *mut usize,
) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_cipher_finish(
    _: *mut CipherOperation,
    _: *mut u8,
    _: usize,
    _: *mut usize,
) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_cipher_abort(_: *mut CipherOperation) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_mac_setup(_: *mut MacOperation, _: u32, _: u32) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_mac_input(_: *mut MacOperation, _: *const u8, _: usize) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_mac_finish(
    _: *mut MacOperation,
    _: *mut u8,
    _: usize,
    _: *mut usize,
) -> i32 {
    -134
}
unsafe extern "C" fn unsupported_mac_abort(_: *mut MacOperation) -> i32 {
    -134
}

static TABLE: PsaTable = PsaTable {
    version: VERSION,
    size: mem::size_of::<PsaTable>() as u32,
    psa_hash_compute: hash_compute,
    psa_hash_setup: hash_setup,
    psa_hash_update: hash_update,
    psa_hash_finish: hash_finish,
    psa_hash_abort: hash_abort,
    psa_import_key: import_key,
    psa_destroy_key: destroy_key,
    psa_verify_hash: verify_hash,
    psa_generate_random: unsupported_random,
    psa_generate_key: unsupported_generate,
    psa_export_public_key: unsupported_export,
    psa_raw_key_agreement: unsupported_agreement,
    psa_reset_key_attributes: reset_attributes,
    psa_cipher_encrypt_setup: unsupported_cipher_setup,
    psa_cipher_decrypt_setup: unsupported_cipher_setup,
    psa_cipher_set_iv: unsupported_cipher_input,
    psa_cipher_update: unsupported_cipher_update,
    psa_cipher_finish: unsupported_cipher_finish,
    psa_cipher_abort: unsupported_cipher_abort,
    psa_mac_sign_setup: unsupported_mac_setup,
    psa_mac_update: unsupported_mac_input,
    psa_mac_sign_finish: unsupported_mac_finish,
    psa_mac_abort: unsupported_mac_abort,
};
pub(super) fn psa() -> Psa {
    unsafe { Psa::from_table(&TABLE) }.unwrap()
}
pub(super) unsafe extern "C" fn get_component(_: *mut c_void, name: *const c_char) -> *const c_void {
    if !name.is_null() && CStr::from_ptr(name).to_bytes() == b"psa" {
        (&TABLE as *const PsaTable).cast()
    } else {
        core::ptr::null()
    }
}
fn no_resources() {
    HASHES.with(|n| assert_eq!(n.get(), 0, "hash operation leaked"));
    KEYS.with(|keys| assert!(keys.borrow().is_empty(), "imported key leaked"));
}
#[test]
fn native_table_hash_vectors_and_stream_drop() {
    let psa = psa();
    assert_eq!(
        psa.sha256(b"abc").unwrap(),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad
        ]
    );
    let mut hash = psa.sha256_stream().unwrap();
    hash.update(b"a").unwrap();
    hash.update(b"bc").unwrap();
    assert_eq!(hash.finish().unwrap(), psa.sha256(b"abc").unwrap());
    drop(hash);
    let hash = psa.sha256_stream().unwrap();
    drop(hash); // Disconnect before finish.
    FAIL_SETUP.with(|f| f.set(true));
    assert!(psa.sha256_stream().is_err());
    assert_eq!(psa.sha256(b"").unwrap(), SoftwareSha256::digest(b"")[..]);
    no_resources();
}
#[test]
fn native_table_verification_always_destroys_imported_keys() {
    use p256::ecdsa::{signature::Signer, SigningKey};
    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    let public_key: [u8; 65] = key
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .unwrap();
    let signature: Signature = key.sign(b"signed header");
    let signature: [u8; 64] = signature.to_bytes().into();
    let psa = psa();
    assert!(psa
        .verify_p256_sha256(&public_key, &signature, b"signed header")
        .is_ok());
    assert_eq!(
        psa.verify_p256_sha256(&public_key, &signature, b"changed"),
        Err(ERROR_INVALID_SIGNATURE)
    );
    let mut bad = signature;
    bad[0] ^= 1;
    assert!(psa
        .verify_p256_sha256(&public_key, &bad, b"signed header")
        .is_err());
    let mut bad = public_key;
    bad[0] = 0;
    assert_eq!(
        psa.verify_p256_sha256(&bad, &signature, b"signed header"),
        Err(ERROR_INVALID_ARGUMENT)
    );
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
    assert!(hw
        .get_component(CStr::from_bytes_with_nul(b"unknown\0").unwrap())
        .is_null());
    host.size = mem::offset_of!(xip_abi::Host, get_component) as u32;
    let hw = unsafe { crate::Hw::new(&host) }.unwrap();
    assert!(Psa::from_host(hw).is_none());
    let prefix = [VERSION, 8];
    assert!(unsafe { Psa::from_table(prefix.as_ptr().cast()) }.is_none());
}

#[test]
fn rustcrypto_digest_uses_original_trait_and_cleans_up_on_failure() {
    // This is the same Digest import and method sequence as sha2::Sha256.
    use xip_abi::psa::rustcrypto::{bind, Digest, Sha256};
    bind(&TABLE).unwrap();
    bind(&TABLE).unwrap(); // Idempotent initialization.
    fn hash<D: Digest>(data: &[u8]) -> xip_abi::psa::rustcrypto::digest::Output<D> {
        D::digest(data)
    }
    for len in [0, 1, 63, 64, 65, 128, 1025, 8192] {
        let data = std::vec![0x5a; len];
        let expected = SoftwareSha256::digest(&data);
        assert_eq!(hash::<Sha256>(&data)[..], expected[..]);
        let mut h = Sha256::new();
        for chunk in data.chunks(17) {
            h.update(chunk);
        }
        assert_eq!(h.finalize()[..], expected[..]);
        let mut h = Sha256::new_with_prefix(&data);
        assert_eq!(h.finalize_reset()[..], expected[..]);
        h.update(b"abc");
        assert_eq!(h.finalize()[..], SoftwareSha256::digest(b"abc")[..]);
        let mut h = Sha256::new_with_prefix(b"discard");
        Digest::reset(&mut h);
        assert_eq!(h.chain_update(&data).finalize()[..], expected[..]);
    }
    FAIL_SETUP.with(|f| f.set(true));
    assert!(std::panic::catch_unwind(Sha256::new).is_err());
    no_resources();
}

#[test]
fn rustcrypto_p256_preserves_encoding_traits_errors_and_cleanup() {
    use xip_abi::psa::rustcrypto::{
        bind,
        ecdsa::{
            signature::{hazmat::PrehashVerifier, Verifier},
            Signature, VerifyingKey,
        },
    };
    use p256::ecdsa::{
        signature::{hazmat::PrehashSigner, Signer},
        SigningKey,
    };
    bind(&TABLE).unwrap();
    let signing = SigningKey::from_bytes((&[3u8; 32]).into()).unwrap();
    let message = b"same RustCrypto methods, PSA implementation";
    let signature: Signature = signing.sign(message);
    for compressed in [false, true] {
        let point = signing.verifying_key().to_encoded_point(compressed);
        let key = VerifyingKey::from_sec1_bytes(point.as_bytes()).unwrap();
        fn check<V: Verifier<Signature>>(
            key: &V,
            message: &[u8],
            sig: &Signature,
        ) -> Result<(), p256::ecdsa::Error> {
            key.verify(message, sig)
        }
        check(&key, message, &signature).unwrap();
        assert!(check(&key, b"tampered", &signature).is_err());
        let hash = SoftwareSha256::digest(message);
        key.verify_prehash(&hash, &signature).unwrap();
        for len in [16, 24, 32, 48, 64] {
            let prehash = std::vec![0x42; len];
            let signature: Signature = signing.sign_prehash(&prehash).unwrap();
            key.verify_prehash(&prehash, &signature).unwrap();
        }
        assert!(key.verify_prehash(&[0; 15], &signature).is_err());
        drop(key);
        no_resources();
    }
    assert!(VerifyingKey::from_sec1_bytes(&[0; 65]).is_err());
    no_resources();
}
