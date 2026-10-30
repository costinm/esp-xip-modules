//! Persistent device signing identity. The private key never leaves the runtime.
use core::ffi::{c_int, c_void};

pub const IDENTITY_VERSION: u32 = 1;
/// `get_component("identity")`: a P-256 identity (on the ESP the existing `sec/id_p256` key; on a host `identity.p256` in the state directory).
/// Results are written into packets like those of [`crate::Crypto`]. Verification is `Crypto::verify`.
#[repr(C)]
pub struct Identity {
    /// 1.
    pub version: u32,
    /// Size of this table.
    pub size: u32,
    /// Write the uncompressed 65-byte SEC1 public key at `offset` of `packet`, creating and committing
    /// the identity on first use. No volatile identity is returned on failure.
    pub public_key: Option<unsafe extern "C" fn(ctx: *mut c_void, packet: u32, offset: u32) -> c_int>,
    /// SHA-256 and sign `len` bytes of `packet` at `offset`; the raw 64-byte r||s signature goes to `out_offset` of `out_packet`.
    /// Call when cached signed facts change, never for periodic publication.
    pub sign: Option<unsafe extern "C" fn(ctx: *mut c_void, packet: u32, offset: u32, len: u32, out_packet: u32, out_offset: u32) -> c_int>,
}

#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Identity>() == 16);

impl crate::crypto::CryptoClient {
    /// The device's public key (65 bytes, uncompressed).
    pub fn identity_public_key(&self) -> Result<[u8; 65], i32> {
        let p = self.hw.get_component(c"identity").cast::<Identity>();
        let table = unsafe { p.as_ref() }.filter(|t| t.version == IDENTITY_VERSION && t.size as usize >= core::mem::size_of::<Identity>()).ok_or(crate::ERR_UNSUPPORTED)?;
        let f = table.public_key.ok_or(crate::ERR_UNSUPPORTED)?;
        let s = self.packet(&[])?;
        let rc = unsafe { f(self.ctx(), s.pkt, 0) };
        if rc < 0 {
            return Err(rc);
        }
        let mut out = [0u8; 65];
        self.read(&s, 0, &mut out)?;
        Ok(out)
    }
    /// Sign `message` (it must fit one packet) with the device identity.
    pub fn identity_sign(&self, message: &[u8]) -> Result<[u8; 64], i32> {
        let p = self.hw.get_component(c"identity").cast::<Identity>();
        let table = unsafe { p.as_ref() }.filter(|t| t.version == IDENTITY_VERSION && t.size as usize >= core::mem::size_of::<Identity>()).ok_or(crate::ERR_UNSUPPORTED)?;
        let f = table.sign.ok_or(crate::ERR_UNSUPPORTED)?;
        let s = self.packet(message)?;
        let rc = unsafe { f(self.ctx(), s.pkt, 0, message.len() as u32, s.pkt, message.len() as u32) };
        if rc < 0 {
            return Err(rc);
        }
        let mut out = [0u8; 64];
        self.read(&s, message.len(), &mut out)?;
        Ok(out)
    }
}
