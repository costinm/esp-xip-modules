//! The runtime's cryptography in pure Rust (RustCrypto), as plain byte functions. The runtimes adapt packets to it (`crypto` and `identity`
//! tables): modules never see keys, contexts or a crypto library, and the runtime itself (QUIC protection, discovery signatures) calls it
//! directly. Hash algorithms are the PSA numbers so a C provider (`psa_hash_*`) answers the same calls.
use aes_gcm::{
    aead::AeadInPlace,
    aes::{
        cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit as _},
        Aes128, Aes256,
    },
    Aes128Gcm, Aes256Gcm, Nonce, Tag,
};
use p256::ecdsa::signature::hazmat::PrehashVerifier;
use p256::ecdsa::{signature::Signer, signature::Verifier, Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::sync::OnceLock;

pub const ALG_SHA_256: u32 = 0x0200_0009;
pub const ALG_SHA_384: u32 = 0x0200_000a;
pub const ALG_SHA_512: u32 = 0x0200_000b;
/// ECDSA over P-256 with SHA-256 (message signed or verified, raw r||s).
pub const ALG_ECDSA_SHA_256: u32 = 0x0600_0609;
/// AES-GCM AEAD (12-byte nonce, 16-byte tag).
pub const ALG_GCM: u32 = 0x0550_0200;
/// AES-CCM AEAD (12-byte nonce, 16-byte tag; `PSA_ALG_CCM`).
pub const ALG_CCM: u32 = 0x0550_0100;

/// A multipart hash.
pub enum Hasher {
    S256(Sha256),
    S384(Sha384),
    S512(Sha512),
}
impl Hasher {
    pub fn new(alg: u32) -> Option<Hasher> {
        Some(match alg {
            ALG_SHA_256 => Hasher::S256(Sha256::new()),
            ALG_SHA_384 => Hasher::S384(Sha384::new()),
            ALG_SHA_512 => Hasher::S512(Sha512::new()),
            _ => return None,
        })
    }
    pub fn update(&mut self, d: &[u8]) {
        match self {
            Hasher::S256(h) => h.update(d),
            Hasher::S384(h) => h.update(d),
            Hasher::S512(h) => h.update(d),
        }
    }
    pub fn finish(self) -> Vec<u8> {
        match self {
            Hasher::S256(h) => h.finalize().to_vec(),
            Hasher::S384(h) => h.finalize().to_vec(),
            Hasher::S512(h) => h.finalize().to_vec(),
        }
    }
}
/// The hash of `data`, or `None` for an algorithm that is not supported.
pub fn hash(alg: u32, data: &[u8]) -> Option<Vec<u8>> {
    let mut h = Hasher::new(alg)?;
    h.update(data);
    Some(h.finish())
}

/// HMAC-SHA-256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = if key.len() > 64 { Sha256::digest(key).to_vec() } else { key.to_vec() };
    k.resize(64, 0);
    let mut inner = Sha256::new();
    inner.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<_>>());
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<_>>());
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Verify the raw r||s ECDSA/SHA-256 `signature` of `message` with a compressed (33) or uncompressed (65 byte) SEC1 public key.
pub fn verify_p256_sha256(public: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let (Ok(key), Ok(sig)) = (VerifyingKey::from_sec1_bytes(public), Signature::from_slice(signature)) else { return false };
    key.verify(message, &sig).is_ok()
}
/// As [`verify_p256_sha256`] over an already computed SHA-256 `hash`.
pub fn verify_p256_prehash(public: &[u8], hash: &[u8], signature: &[u8]) -> bool {
    let (Ok(key), Ok(sig)) = (VerifyingKey::from_sec1_bytes(public), Signature::from_slice(signature)) else { return false };
    key.verify_prehash(hash, &sig).is_ok()
}

/// A P-256 key pair for key agreement: the uncompressed public key and the raw ECDH secret with a peer's public key (SEC1).
pub struct Agreement(p256::SecretKey);
impl Agreement {
    pub fn generate() -> Agreement {
        Agreement(p256::SecretKey::random(&mut OsRng))
    }
    pub fn public(&self) -> Vec<u8> {
        self.0.public_key().to_encoded_point(false).as_bytes().to_vec()
    }
    pub fn secret(&self, peer: &[u8]) -> Option<[u8; 32]> {
        let peer = p256::PublicKey::from_sec1_bytes(peer).ok()?;
        Some(p256::ecdh::diffie_hellman(self.0.to_nonzero_scalar(), peer.as_affine()).raw_secret_bytes().as_slice().try_into().unwrap())
    }
}

pub fn random(out: &mut [u8]) {
    OsRng.fill_bytes(out);
}

fn ccm_encrypt_block<C: BlockEncrypt>(cipher: &C, block: &mut [u8; 16]) {
    cipher.encrypt_block(GenericArray::from_mut_slice(block));
}

fn ccm_cbc_mac<C: BlockEncrypt>(cipher: &C, nonce: &[u8; 12], aad: &[u8], pt: &[u8]) -> Option<[u8; 16]> {
    if pt.len() > 0x00ff_ffff || aad.len() >= 0xff00 {
        return None;
    }
    let mut mac = [0u8; 16];
    mac[0] = (if aad.is_empty() { 0x00 } else { 0x40 }) | 0x38 | 0x02;
    mac[1..13].copy_from_slice(nonce);
    let len_u = pt.len() as u32;
    mac[13] = ((len_u >> 16) & 0xff) as u8;
    mac[14] = ((len_u >> 8) & 0xff) as u8;
    mac[15] = (len_u & 0xff) as u8;
    ccm_encrypt_block(cipher, &mut mac);

    if !aad.is_empty() {
        let mut blk = [0u8; 16];
        let aad_len = aad.len() as u16;
        blk[0] = (aad_len >> 8) as u8;
        blk[1] = (aad_len & 0xff) as u8;
        let mut pos = 2usize;
        for &b in aad {
            blk[pos] = b;
            pos += 1;
            if pos == 16 {
                for (m, &x) in mac.iter_mut().zip(blk.iter()) {
                    *m ^= x;
                }
                ccm_encrypt_block(cipher, &mut mac);
                blk = [0u8; 16];
                pos = 0;
            }
        }
        if pos > 0 {
            for (m, &x) in mac.iter_mut().zip(blk.iter()) {
                *m ^= x;
            }
            ccm_encrypt_block(cipher, &mut mac);
        }
    }

    let mut chunks = pt.chunks_exact(16);
    for chunk in &mut chunks {
        for (m, &x) in mac.iter_mut().zip(chunk.iter()) {
            *m ^= x;
        }
        ccm_encrypt_block(cipher, &mut mac);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        for (m, &x) in mac.iter_mut().zip(rem.iter()) {
            *m ^= x;
        }
        ccm_encrypt_block(cipher, &mut mac);
    }
    Some(mac)
}

fn ccm_ctr_xor<C: BlockEncrypt>(cipher: &C, nonce: &[u8; 12], body: &mut [u8]) {
    let mut ctr = [0u8; 16];
    ctr[0] = 0x02;
    ctr[1..13].copy_from_slice(nonce);
    let mut block_idx = 1u32;
    for chunk in body.chunks_mut(16) {
        ctr[13] = ((block_idx >> 16) & 0xff) as u8;
        ctr[14] = ((block_idx >> 8) & 0xff) as u8;
        ctr[15] = (block_idx & 0xff) as u8;
        let mut ks = ctr;
        ccm_encrypt_block(cipher, &mut ks);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= *k;
        }
        block_idx = block_idx.wrapping_add(1);
    }
}

fn ccm_s0<C: BlockEncrypt>(cipher: &C, nonce: &[u8; 12]) -> [u8; 16] {
    let mut s0 = [0u8; 16];
    s0[0] = 0x02;
    s0[1..13].copy_from_slice(nonce);
    ccm_encrypt_block(cipher, &mut s0);
    s0
}

fn ccm_seal_with_cipher<C: BlockEncrypt>(cipher: &C, nonce: &[u8; 12], aad: &[u8], body: &mut [u8]) -> Option<[u8; 16]> {
    let mut tag = ccm_cbc_mac(cipher, nonce, aad, body)?;
    ccm_ctr_xor(cipher, nonce, body);
    let s0 = ccm_s0(cipher, nonce);
    for (t, &s) in tag.iter_mut().zip(s0.iter()) {
        *t ^= s;
    }
    Some(tag)
}

fn ccm_open_with_cipher<C: BlockEncrypt>(cipher: &C, nonce: &[u8; 12], aad: &[u8], body: &mut [u8], tag: &[u8; 16]) -> bool {
    ccm_ctr_xor(cipher, nonce, body);
    let Some(mac) = ccm_cbc_mac(cipher, nonce, aad, body) else {
        body.fill(0);
        return false;
    };
    let s0 = ccm_s0(cipher, nonce);
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= (mac[i] ^ s0[i]) ^ tag[i];
    }
    if diff == 0 {
        true
    } else {
        body.fill(0);
        false
    }
}

/// In-place AES-128/256-GCM or AES-128/256-CCM authenticated encryption matching `psa_aead_encrypt`.
pub fn aead_seal_in_place(alg: u32, key: &[u8], nonce: &[u8; 12], aad: &[u8], body: &mut [u8]) -> Option<[u8; 16]> {
    match alg {
        ALG_GCM => {
            let nonce = Nonce::from_slice(nonce);
            let tag = match key.len() {
                16 => Aes128Gcm::new_from_slice(key).ok()?.encrypt_in_place_detached(nonce, aad, body).ok()?,
                32 => Aes256Gcm::new_from_slice(key).ok()?.encrypt_in_place_detached(nonce, aad, body).ok()?,
                _ => return None,
            };
            let mut out = [0u8; 16];
            out.copy_from_slice(&tag);
            Some(out)
        }
        ALG_CCM => match key.len() {
            16 => ccm_seal_with_cipher(&Aes128::new_from_slice(key).ok()?, nonce, aad, body),
            32 => ccm_seal_with_cipher(&Aes256::new_from_slice(key).ok()?, nonce, aad, body),
            _ => None,
        },
        _ => None,
    }
}

/// In-place AES-128/256-GCM or AES-128/256-CCM authenticated decryption matching `psa_aead_decrypt`.
pub fn aead_open_in_place(alg: u32, key: &[u8], nonce: &[u8; 12], aad: &[u8], body: &mut [u8], tag: &[u8; 16]) -> bool {
    match alg {
        ALG_GCM => {
            let nonce = Nonce::from_slice(nonce);
            let tag = Tag::from_slice(tag);
            match key.len() {
                16 => Aes128Gcm::new_from_slice(key).is_ok_and(|c| c.decrypt_in_place_detached(nonce, aad, body, tag).is_ok()),
                32 => Aes256Gcm::new_from_slice(key).is_ok_and(|c| c.decrypt_in_place_detached(nonce, aad, body, tag).is_ok()),
                _ => false,
            }
        }
        ALG_CCM => match key.len() {
            16 => Aes128::new_from_slice(key).is_ok_and(|c| ccm_open_with_cipher(&c, nonce, aad, body, tag)),
            32 => Aes256::new_from_slice(key).is_ok_and(|c| ccm_open_with_cipher(&c, nonce, aad, body, tag)),
            _ => false,
        },
        _ => false,
    }
}

// ---- identity: the device key of this process (P-256) ----

static IDENTITY_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Keep the device key in `dir/identity.p256` (the 32-byte scalar, mode 0600), created on first use: the identity then survives restarts.
/// Call it before the first use of the identity (it is a no-op afterwards); a library user (lmesh, Android) passes its own state directory,
/// `xip-host` takes `XIP_STATE_DIR`. Without it the key lives in memory for the process.
pub fn set_identity_dir(dir: impl Into<std::path::PathBuf>) -> Result<(), std::io::Error> {
    let dir = dir.into();
    std::fs::create_dir_all(&dir)?;
    let _ = IDENTITY_DIR.set(dir);
    Ok(())
}

fn load_or_create(dir: &std::path::Path) -> std::io::Result<SigningKey> {
    let file = dir.join("identity.p256");
    match std::fs::read(&file) {
        Ok(raw) => SigningKey::from_slice(&raw).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "identity.p256 is not a P-256 key")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let key = SigningKey::random(&mut OsRng);
            // written whole under a temporary name, then renamed: a crash never leaves half a key
            let tmp = dir.join("identity.p256.tmp");
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&key.to_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &file)?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

fn device_key() -> &'static SigningKey {
    static K: OnceLock<SigningKey> = OnceLock::new();
    K.get_or_init(|| match IDENTITY_DIR.get() {
        // a stored key that cannot be read or written is an error to the operator, not a reason to run with another identity
        Some(dir) => load_or_create(dir).unwrap_or_else(|e| panic!("identity in {}: {e}", dir.display())),
        None => SigningKey::random(&mut OsRng),
    })
}
/// The uncompressed (65 byte) public key of the device identity, created on first use.
pub fn identity_public_key() -> [u8; 65] {
    VerifyingKey::from(device_key()).to_encoded_point(false).as_bytes().try_into().unwrap()
}
/// Sign `message` (SHA-256, ECDSA) with the device identity: raw r||s.
pub fn identity_sign(message: &[u8]) -> [u8; 64] {
    let s: Signature = device_key().sign(message);
    s.to_bytes().into()
}
