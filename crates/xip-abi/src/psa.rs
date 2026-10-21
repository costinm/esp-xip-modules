//! Hand-written wrapper for `include/xip_psa.h`, using native PSA signatures.
//! Context layouts match ESP-IDF 6.0.2; firmware checks the C copies against its
//! actual SDK. Function descriptions summarize the native PSA header.
//! RustCrypto-compatible adapters are in `psa::rustcrypto` when that feature is enabled.
use crate::Hw;
use core::{ffi::c_void, mem};

pub const VERSION: u32 = 1;
pub const ALG_SHA_256: u32 = 0x0200_0009;
pub const ALG_ECDSA_SHA_256: u32 = 0x0600_0609;
pub const KEY_TYPE_ECC_PUBLIC_KEY_SECP_R1: u16 = 0x4112;
pub const KEY_USAGE_VERIFY_HASH: u32 = 0x2000;
pub const KEY_USAGE_VERIFY_MESSAGE: u32 = 0x0800;
pub const ERROR_INVALID_ARGUMENT: i32 = -135;
pub const ERROR_BAD_STATE: i32 = -137;
pub const ERROR_INSUFFICIENT_MEMORY: i32 = -141;
pub const ERROR_INVALID_SIGNATURE: i32 = -149;
pub const ERROR_CORRUPTION_DETECTED: i32 = -151;

#[cfg(not(any(xip_psa_esp32, xip_psa_esp32s3)))]
#[repr(C)]
pub struct Sha512Context {
    pub total: [u64; 2],
    pub state: [u64; 8],
    pub buffer: [u8; 128],
    pub is384: i32,
}
#[cfg(xip_psa_esp32)]
#[repr(C)]
pub struct Sha256Context {
    pub buffer: [u8; 64],
    pub total: [u32; 2],
    pub state: [u32; 8],
    pub is224: i32,
}
#[repr(C)]
pub struct BuiltinHashContext {
    pub alg: u32,
    #[cfg(not(any(xip_psa_esp32, xip_psa_esp32s3)))]
    pub ctx: Sha512Context,
    #[cfg(xip_psa_esp32)]
    pub ctx: Sha256Context,
    /// ESP32-S3: hardware SHA; what is left of the software contexts (size and alignment from the SDK).
    #[cfg(xip_psa_esp32s3)]
    pub ctx: [u8; 84],
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EspHashContext {
    pub sha_ctx: *mut c_void,
    pub sha_type: i32,
}
#[repr(C)]
pub union DriverHashContext {
    pub dummy: u32,
    pub mbedtls_ctx: mem::ManuallyDrop<BuiltinHashContext>,
    pub esp_ctx: EspHashContext,
}
#[repr(C)]
pub struct HashOperation {
    pub id: u32,
    pub ctx: DriverHashContext,
}
impl Default for HashOperation {
    fn default() -> Self {
        // Native PSA_HASH_OPERATION_INIT: driver id and all union bytes zero.
        unsafe { mem::zeroed() }
    }
}
#[repr(C)]
#[derive(Default)]
pub struct KeyPolicy {
    pub usage: u32,
    pub alg: u32,
    pub alg2: u32,
}
#[repr(C)]
#[derive(Default)]
pub struct KeyAttributes {
    pub key_type: u16,
    pub bits: u16,
    pub lifetime: u32,
    pub policy: KeyPolicy,
    pub id: u32,
}
/// Ordinary component table; these are the exact native PSA function signatures.
#[repr(C)]
pub struct PsaTable {
    pub version: u32,
    pub size: u32,
    /// Calculate the hash of a message in one shot.
    pub psa_hash_compute:
        unsafe extern "C" fn(u32, *const u8, usize, *mut u8, usize, *mut usize) -> i32,
    /// Set up a multipart hash operation; initialize the operation to zero first.
    pub psa_hash_setup: unsafe extern "C" fn(*mut HashOperation, u32) -> i32,
    /// Add a message fragment to a multipart hash operation.
    pub psa_hash_update: unsafe extern "C" fn(*mut HashOperation, *const u8, usize) -> i32,
    /// Finish calculating a hash; successful completion releases the operation.
    pub psa_hash_finish:
        unsafe extern "C" fn(*mut HashOperation, *mut u8, usize, *mut usize) -> i32,
    /// Abort a hash operation and release its resources; safe for an inactive operation.
    pub psa_hash_abort: unsafe extern "C" fn(*mut HashOperation) -> i32,
    /// Import a key from its binary representation using the supplied attributes.
    pub psa_import_key:
        unsafe extern "C" fn(*const KeyAttributes, *const u8, usize, *mut u32) -> i32,
    /// Destroy a key and release its key-slot resources.
    pub psa_destroy_key: unsafe extern "C" fn(u32) -> i32,
    /// Verify a signature over an already calculated hash.
    pub psa_verify_hash: unsafe extern "C" fn(u32, u32, *const u8, usize, *const u8, usize) -> i32,
    /// Generate cryptographically secure random bytes.
    pub psa_generate_random: unsafe extern "C" fn(*mut u8, usize) -> i32,
    /// Generate a key or key pair according to the supplied attributes.
    pub psa_generate_key: unsafe extern "C" fn(*const KeyAttributes, *mut u32) -> i32,
    /// Export the public part of a key in its standard binary representation.
    pub psa_export_public_key: unsafe extern "C" fn(u32, *mut u8, usize, *mut usize) -> i32,
    /// Perform raw key agreement without applying a key-derivation function.
    pub psa_raw_key_agreement:
        unsafe extern "C" fn(u32, u32, *const u8, usize, *mut u8, usize, *mut usize) -> i32,
    /// Reset attributes and release any associated resources.
    pub psa_reset_key_attributes: unsafe extern "C" fn(*mut KeyAttributes),
    /// Set up a multipart symmetric encryption operation.
    pub psa_cipher_encrypt_setup: unsafe extern "C" fn(*mut CipherOperation, u32, u32) -> i32,
    /// Set up a multipart symmetric decryption operation.
    pub psa_cipher_decrypt_setup: unsafe extern "C" fn(*mut CipherOperation, u32, u32) -> i32,
    /// Set the initialization vector for a multipart cipher operation.
    pub psa_cipher_set_iv: unsafe extern "C" fn(*mut CipherOperation, *const u8, usize) -> i32,
    /// Encrypt or decrypt a message fragment, writing the produced output.
    pub psa_cipher_update: unsafe extern "C" fn(
        *mut CipherOperation,
        *const u8,
        usize,
        *mut u8,
        usize,
        *mut usize,
    ) -> i32,
    /// Finish a cipher operation, writing any remaining output.
    pub psa_cipher_finish:
        unsafe extern "C" fn(*mut CipherOperation, *mut u8, usize, *mut usize) -> i32,
    /// Abort a cipher operation and release its resources.
    pub psa_cipher_abort: unsafe extern "C" fn(*mut CipherOperation) -> i32,
    /// Set up a multipart message-authentication-code calculation.
    pub psa_mac_sign_setup: unsafe extern "C" fn(*mut MacOperation, u32, u32) -> i32,
    /// Add a message fragment to a multipart MAC operation.
    pub psa_mac_update: unsafe extern "C" fn(*mut MacOperation, *const u8, usize) -> i32,
    /// Finish calculating a message authentication code.
    pub psa_mac_sign_finish:
        unsafe extern "C" fn(*mut MacOperation, *mut u8, usize, *mut usize) -> i32,
    /// Abort a MAC operation and release its resources.
    pub psa_mac_abort: unsafe extern "C" fn(*mut MacOperation) -> i32,
}

/// Native multipart contexts are implementation-defined. Use matching SDK
/// bindings for their storage; this wrapper only supplies pointer signatures.
#[repr(C)]
pub struct CipherOperation {
    _private: [u8; 0],
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,
}
#[repr(C)]
pub struct MacOperation {
    _private: [u8; 0],
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,
}
impl PsaTable {
    /// Look up the firmware-owned native PSA table through get_component("psa").
    pub fn from_host(hw: Hw) -> Option<&'static Self> {
        let name = core::ffi::CStr::from_bytes_with_nul(b"psa\0").unwrap();
        unsafe { Self::from_table(hw.get_component(name).cast()) }
    }
    /// Validate the version and complete table size before accessing its fields.
    ///
    /// # Safety
    /// The pointer must be NULL or point to a readable, aligned version/size
    /// prefix. A supported complete table must remain valid for the firmware
    /// lifetime, with native function pointers and matching SDK context layouts.
    pub unsafe fn from_table(table: *const Self) -> Option<&'static Self> {
        if table.is_null()
            || table.cast::<u32>().read() != VERSION
            || table.cast::<u32>().add(1).read() < mem::size_of::<Self>() as u32
        {
            return None;
        }
        Some(&*table)
    }
}
#[cfg(xip_psa_esp32)]
const _: () = {
    assert!(mem::size_of::<HashOperation>() == 116);
    assert!(mem::align_of::<HashOperation>() == 4);
    assert!(mem::offset_of!(HashOperation, ctx) == 4);
};
#[cfg(xip_psa_esp32s3)]
const _: () = {
    assert!(mem::size_of::<HashOperation>() == 92);
    assert!(mem::align_of::<HashOperation>() == 4);
    assert!(mem::offset_of!(HashOperation, ctx) == 4);
};
#[cfg(all(target_arch = "riscv32", not(xip_psa_esp32)))]
const _: () = {
    assert!(mem::size_of::<HashOperation>() == 232);
    assert!(mem::align_of::<HashOperation>() == 8);
    assert!(mem::offset_of!(HashOperation, ctx) == 8);
};
const _: () = assert!(mem::size_of::<KeyAttributes>() == 24);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(mem::size_of::<PsaTable>() == 100);

#[cfg(feature = "rustcrypto")]
pub mod rustcrypto;
