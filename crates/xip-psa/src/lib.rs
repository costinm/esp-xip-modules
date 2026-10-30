//! The runtime's cryptography in pure Rust (RustCrypto), as plain byte functions. The runtimes adapt packets to it (`crypto` and `identity`
//! tables): modules never see keys, contexts or a crypto library, and the runtime itself (QUIC protection, discovery signatures) calls it
//! directly. Hash algorithms are the PSA numbers so a C provider (`psa_hash_*`) answers the same calls.
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
