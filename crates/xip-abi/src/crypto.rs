//! The runtime's cryptography, for modules: calls on packets. A module never holds keys or a crypto library, and no native
//! context layout is part of the ABI: data is a byte range of a pool packet (`packet`, `offset`, `len`; `offset` counts from the start of the
//! payload window `pkt_buf` returns), results are written into a packet, and an operation in progress is a handle owned by the runtime
//! (released by `hash_finish`, `hash_abort` or the end of the module). The runtime protects its own traffic with the same code.
//! Algorithms are the PSA numbers ([`ALG_SHA_256`]...), so the ESP runtime answers them with `psa_*`.
use core::ffi::{c_int, c_void};

pub const CRYPTO_VERSION: u32 = 1;
/// SHA-256 (digest 32 bytes).
pub const ALG_SHA_256: u32 = 0x0200_0009;
/// SHA-384 (digest 48 bytes).
pub const ALG_SHA_384: u32 = 0x0200_000a;
/// SHA-512 (digest 64 bytes).
pub const ALG_SHA_512: u32 = 0x0200_000b;
/// ECDSA over P-256 with SHA-256: `verify` checks a message against a raw 64-byte r||s signature.
pub const ALG_ECDSA_SHA_256: u32 = 0x0600_0609;

/// `get_component("crypto")`. Every packet argument is a handle the caller holds; ranges outside the payload window answer `ERR_ARG`.
/// A signature that does not verify answers `ERR_AUTH`.
#[repr(C)]
pub struct Crypto {
    /// 1.
    pub version: u32,
    /// Size of this table.
    pub size: u32,
    /// Hash `len` bytes of `packet` at `offset` and write the digest at `out_offset` of `out_packet` (may be the same packet, beyond the input).
    /// Returns the digest length.
    pub hash: Option<unsafe extern "C" fn(ctx: *mut c_void, alg: u32, packet: u32, offset: u32, len: u32, out_packet: u32, out_offset: u32) -> c_int>,
    /// Start a multipart hash. Returns its handle (positive).
    pub hash_start: Option<unsafe extern "C" fn(ctx: *mut c_void, alg: u32) -> c_int>,
    /// Add a byte range of a packet to the hash `handle`.
    pub hash_update: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: u32, packet: u32, offset: u32, len: u32) -> c_int>,
    /// Finish the hash `handle` (it is released) and write the digest at `out_offset` of `out_packet`. Returns the digest length.
    pub hash_finish: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: u32, out_packet: u32, out_offset: u32) -> c_int>,
    /// Release the hash `handle` without a result. Safe for an unknown handle.
    pub hash_abort: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: u32) -> c_int>,
    /// Verify with `alg` ([`ALG_ECDSA_SHA_256`]): the range at `offset` holds the public key (`key_len`: 33 compressed or 65 uncompressed SEC1 bytes),
    /// then the message (`message_len`), then the 64-byte raw signature (`signature_len`). 0 when valid, `ERR_AUTH` when not.
    pub verify: Option<unsafe extern "C" fn(ctx: *mut c_void, alg: u32, packet: u32, offset: u32, key_len: u32, message_len: u32, signature_len: u32) -> c_int>,
    /// Fill `len` bytes of `packet` at `offset` with random bytes.
    pub random: Option<unsafe extern "C" fn(ctx: *mut c_void, packet: u32, offset: u32, len: u32) -> c_int>,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Crypto>() == 36);

/// Digest length of a supported hash algorithm.
pub const fn digest_len(alg: u32) -> Option<usize> {
    match alg {
        ALG_SHA_256 => Some(32),
        ALG_SHA_384 => Some(48),
        ALG_SHA_512 => Some(64),
        _ => None,
    }
}

/// The `crypto` and `identity` tables through one handle, with the packet handling done: bytes in, bytes out, one temporary packet per call.
/// Input longer than a packet is hashed in pieces. It is the only way a module should reach cryptography.
#[derive(Clone, Copy)]
pub struct CryptoClient {
    pub(crate) hw: crate::Hw,
    crypto: &'static Crypto,
}
/// The largest range of one packet a client call uses (the rest of the payload window is for the result).
const PIECE: usize = 1024;

impl CryptoClient {
    pub fn from_host(hw: crate::Hw) -> Option<Self> {
        let p = hw.get_component(c"crypto").cast::<Crypto>();
        if p.is_null() {
            return None;
        }
        // SAFETY: a table starts with `{version, size}`; the host keeps it alive
        let table = unsafe { &*p };
        if table.version != CRYPTO_VERSION || (table.size as usize) < core::mem::size_of::<Crypto>() {
            return None;
        }
        Some(Self { hw, crypto: table })
    }
    pub(crate) fn ctx(&self) -> *mut c_void {
        self.hw.ctx()
    }
    /// A packet with `data` at offset 0, to hand to the table (freed by the guard).
    pub(crate) fn packet(&self, data: &[u8]) -> Result<Scratch, i32> {
        let pkt = self.hw.pkt_alloc()?;
        let s = Scratch { hw: self.hw, pkt };
        let (ptr, max) = self.hw.pkt_buf(pkt)?;
        if data.len() > max {
            return Err(crate::ERR_SIZE);
        }
        // SAFETY: `ptr` is the writable window of a packet this module holds, `max` bytes long
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len()) };
        Ok(s)
    }
    pub(crate) fn read(&self, s: &Scratch, offset: usize, out: &mut [u8]) -> Result<(), i32> {
        let (ptr, max) = self.hw.pkt_buf(s.pkt)?;
        if offset + out.len() > max {
            return Err(crate::ERR_SIZE);
        }
        // SAFETY: the range is inside the window
        unsafe { core::ptr::copy_nonoverlapping(ptr.add(offset), out.as_mut_ptr(), out.len()) };
        Ok(())
    }
    pub fn sha256(&self, data: &[u8]) -> Result<[u8; 32], i32> {
        let mut hasher = self.sha256_stream()?;
        hasher.update(data)?;
        hasher.finish()
    }
    /// A multipart SHA-256; dropped unfinished it releases the runtime's state.
    pub fn sha256_stream(&self) -> Result<Sha256, i32> {
        let f = self.crypto.hash_start.ok_or(crate::ERR_UNSUPPORTED)?;
        let rc = unsafe { f(self.ctx(), ALG_SHA_256) };
        if rc <= 0 {
            return Err(if rc < 0 { rc } else { crate::ERR_IO });
        }
        Ok(Sha256 { client: *self, handle: rc as u32 })
    }
    /// ECDSA/SHA-256 over P-256: a 33 or 65 byte SEC1 public key and a raw 64-byte signature. The message must fit one packet beside them.
    pub fn verify_p256_sha256(&self, public_key: &[u8], signature: &[u8; 64], message: &[u8]) -> Result<(), i32> {
        let f = self.crypto.verify.ok_or(crate::ERR_UNSUPPORTED)?;
        let total = public_key.len() + message.len() + signature.len();
        let mut s = self.packet(&[])?;
        let (ptr, max) = self.hw.pkt_buf(s.pkt)?;
        if total > max {
            return Err(crate::ERR_SIZE);
        }
        // SAFETY: three consecutive ranges inside the window
        unsafe {
            core::ptr::copy_nonoverlapping(public_key.as_ptr(), ptr, public_key.len());
            core::ptr::copy_nonoverlapping(message.as_ptr(), ptr.add(public_key.len()), message.len());
            core::ptr::copy_nonoverlapping(signature.as_ptr(), ptr.add(public_key.len() + message.len()), 64);
        }
        s.touch();
        let rc = unsafe { f(self.ctx(), ALG_ECDSA_SHA_256, s.pkt, 0, public_key.len() as u32, message.len() as u32, 64) };
        if rc < 0 { Err(rc) } else { Ok(()) }
    }
    pub fn random(&self, out: &mut [u8]) -> Result<(), i32> {
        let f = self.crypto.random.ok_or(crate::ERR_UNSUPPORTED)?;
        for chunk in out.chunks_mut(PIECE) {
            let s = self.packet(&[])?;
            let rc = unsafe { f(self.ctx(), s.pkt, 0, chunk.len() as u32) };
            if rc < 0 {
                return Err(rc);
            }
            self.read(&s, 0, chunk)?;
        }
        Ok(())
    }
}

pub(crate) struct Scratch {
    hw: crate::Hw,
    pub(crate) pkt: u32,
}
impl Scratch {
    fn touch(&mut self) {}
}
impl Drop for Scratch {
    fn drop(&mut self) {
        self.hw.pkt_free(self.pkt);
    }
}

/// An owned multipart SHA-256 of the runtime.
pub struct Sha256 {
    client: CryptoClient,
    handle: u32,
}
impl Sha256 {
    pub fn update(&mut self, data: &[u8]) -> Result<(), i32> {
        let f = self.client.crypto.hash_update.ok_or(crate::ERR_UNSUPPORTED)?;
        for piece in data.chunks(PIECE) {
            let s = self.client.packet(piece)?;
            let rc = unsafe { f(self.client.ctx(), self.handle, s.pkt, 0, piece.len() as u32) };
            if rc < 0 {
                return Err(rc);
            }
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<[u8; 32], i32> {
        let f = self.client.crypto.hash_finish.ok_or(crate::ERR_UNSUPPORTED)?;
        let s = self.client.packet(&[])?;
        let handle = core::mem::take(&mut self.handle);
        if handle == 0 {
            return Err(crate::ERR_ARG);
        }
        let rc = unsafe { f(self.client.ctx(), handle, s.pkt, 0) };
        if rc != 32 {
            return Err(if rc < 0 { rc } else { crate::ERR_IO });
        }
        let mut out = [0u8; 32];
        self.client.read(&s, 0, &mut out)?;
        Ok(out)
    }
}
impl Drop for Sha256 {
    fn drop(&mut self) {
        if self.handle != 0 {
            if let Some(f) = self.client.crypto.hash_abort {
                unsafe { f(self.client.ctx(), self.handle) };
            }
        }
    }
}
