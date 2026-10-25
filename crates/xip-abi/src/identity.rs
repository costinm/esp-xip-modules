//! Persistent device signing identity. Private key material never leaves the host.
use core::ffi::{c_int, c_void};
/// `get_component("identity")`: P-256 identity, compatible with the firmware's
/// existing `sec/id_p256` key. Calls are serialized by the module owner.
#[repr(C)]
pub struct Identity {
    /// 1.
    pub version: u32,
    /// Size of this table.
    pub size: u32,
    /// Write the uncompressed 65-byte SEC1 public key, creating and committing
    /// the identity on first use. No volatile identity is returned on failure.
    pub public_key: Option<unsafe extern "C" fn(ctx: *mut c_void, out: *mut u8) -> c_int>,
    /// SHA-256 and sign a message, returning a 64-byte raw r||s signature.
    /// Call when cached signed facts change, never for periodic publication.
    pub sign: Option<unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize, signature: *mut u8) -> c_int>,
    /// Verify raw r||s ECDSA/SHA-256 using a compressed (33-byte) or
    /// uncompressed (65-byte) SEC1 key. No device identity is loaded.
    pub verify: Option<unsafe extern "C" fn(ctx: *mut c_void, public: *const u8, public_len: usize,
        data: *const u8, len: usize, signature: *const u8) -> c_int>,
}

#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Identity>() == 20);
