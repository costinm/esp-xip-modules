//! RustCrypto APIs backed by the runtime's native PSA function table.
//!
//! `Sha256` implements `digest` 0.10's real `Digest` trait, with the same
//! new/update/finalize/digest/reset methods and output type as `sha2::Sha256`.
//! `ecdsa::VerifyingKey` provides the previously used P-256 SEC1 constructor
//! and real `signature` 2.x verification traits; `Signature` is the original
//! RustCrypto type. Bind the firmware-lifetime table once before using them.
//! Digest is infallible: missing binding or PSA failures panic. Verification
//! returns the native RustCrypto `signature::Error`, hiding backend error codes.
//! This is the used API subset, not a replacement for all sha2/p256 APIs:
//! no hash Clone (the common table has no psa_hash_clone), signing, or key serde.
use super::*;
use core::sync::atomic::{AtomicPtr, Ordering};
pub use digest::{self, Digest};
use digest::{
    consts::U32, FixedOutput, FixedOutputReset, HashMarker, Output, OutputSizeUser, Reset, Update,
};

static TABLE: AtomicPtr<PsaTable> = AtomicPtr::new(core::ptr::null_mut());

/// Bind a firmware-lifetime native PSA table once. Rebinding the same table is
/// harmless; another table or an incompatible version/size returns BAD_STATE.
/// Modules normally pass `PsaTable::from_host(hw)` during initialization.
pub fn bind(table: &'static PsaTable) -> Result<(), i32> {
    if table.version != VERSION || table.size < mem::size_of::<PsaTable>() as u32 {
        return Err(ERROR_BAD_STATE);
    }
    let ptr = table as *const PsaTable as *mut PsaTable;
    match TABLE.compare_exchange(
        core::ptr::null_mut(),
        ptr,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(previous) if previous == ptr => Ok(()),
        Err(_) => Err(ERROR_BAD_STATE),
    }
}
fn table() -> &'static PsaTable {
    let ptr = TABLE.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "PSA RustCrypto table not bound");
    unsafe { &*ptr }
}
fn success(status: i32) {
    assert_eq!(status, 0, "PSA Digest backend failure");
}

/// `sha2::Sha256`'s Digest methods and output type, implemented with PSA.
/// Native state is aborted on Drop. Backend failures panic, as documented by
/// the infallible RustCrypto Digest contract above.
pub struct Sha256 {
    table: &'static PsaTable,
    operation: HashOperation,
}
impl Default for Sha256 {
    /// Set up a SHA-256 multipart hash operation.
    fn default() -> Self {
        let mut value = Self {
            table: table(),
            operation: HashOperation::default(),
        };
        success(unsafe { (value.table.psa_hash_setup)(&mut value.operation, ALG_SHA_256) });
        value
    }
}
impl OutputSizeUser for Sha256 {
    type OutputSize = U32;
}
impl HashMarker for Sha256 {}
impl Update for Sha256 {
    /// Add a message fragment to the hash operation.
    fn update(&mut self, data: &[u8]) {
        success(unsafe {
            (self.table.psa_hash_update)(&mut self.operation, data.as_ptr(), data.len())
        });
    }
}
impl Sha256 {
    fn finish(&mut self, out: &mut Output<Self>) {
        let mut written = 0;
        success(unsafe {
            (self.table.psa_hash_finish)(
                &mut self.operation,
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        });
        assert_eq!(written, out.len(), "PSA SHA-256 output length");
    }
}
impl FixedOutput for Sha256 {
    /// Finish calculating the hash and consume its state.
    fn finalize_into(mut self, out: &mut Output<Self>) {
        self.finish(out);
    }
}
impl Reset for Sha256 {
    /// Abort the current operation and initialize a fresh SHA-256 operation.
    fn reset(&mut self) {
        success(unsafe { (self.table.psa_hash_abort)(&mut self.operation) });
        self.operation = HashOperation::default();
        success(unsafe { (self.table.psa_hash_setup)(&mut self.operation, ALG_SHA_256) });
    }
}
impl FixedOutputReset for Sha256 {
    /// Finish calculating the hash and reset to accept another message.
    fn finalize_into_reset(&mut self, out: &mut Output<Self>) {
        self.finish(out);
        Reset::reset(self);
    }
}
impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            (self.table.psa_hash_abort)(&mut self.operation);
        }
    }
}

/// P-256 verification API matching the previously used RustCrypto methods.
pub mod ecdsa {
    use super::*;
    pub use p256::ecdsa::{signature, Error, Signature};
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use signature::{hazmat::PrehashVerifier, Verifier};

    /// A P-256 public key imported into PSA; Drop destroys the native key.
    /// Key parsing uses RustCrypto so compressed and uncompressed SEC1 input
    /// have the same accepted encodings. Hashing/verification are performed by PSA.
    pub struct VerifyingKey {
        table: &'static PsaTable,
        key: u32,
    }
    impl VerifyingKey {
        /// Initialize a verifying key from a SEC1-encoded public key.
        /// Matches p256::ecdsa::VerifyingKey::from_sec1_bytes and its Error type.
        pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self, Error> {
            let public_key = p256::PublicKey::from_sec1_bytes(bytes).map_err(|_| Error::new())?;
            let point = public_key.to_encoded_point(false);
            let attributes = KeyAttributes {
                key_type: KEY_TYPE_ECC_PUBLIC_KEY_SECP_R1,
                bits: 256,
                policy: KeyPolicy {
                    usage: KEY_USAGE_VERIFY_HASH | KEY_USAGE_VERIFY_MESSAGE,
                    alg: ALG_ECDSA_SHA_256,
                    alg2: 0,
                },
                ..KeyAttributes::default()
            };
            let table = table();
            let mut key = 0;
            let rc = unsafe {
                (table.psa_import_key)(
                    &attributes,
                    point.as_bytes().as_ptr(),
                    point.as_bytes().len(),
                    &mut key,
                )
            };
            if rc != 0 {
                return Err(Error::new());
            }
            Ok(Self { table, key })
        }
    }
    impl Verifier<Signature> for VerifyingKey {
        /// Verify an ECDSA/SHA-256 signature over a message using native PSA.
        fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), Error> {
            // Verifier is fallible; do not route PSA hash errors through Digest's panic contract.
            let mut hash = [0; 32];
            let mut written = 0;
            let rc = unsafe {
                (self.table.psa_hash_compute)(
                    ALG_SHA_256,
                    message.as_ptr(),
                    message.len(),
                    hash.as_mut_ptr(),
                    hash.len(),
                    &mut written,
                )
            };
            if rc != 0 || written != hash.len() {
                return Err(Error::new());
            }
            self.verify_prehash(&hash, signature)
        }
    }
    impl PrehashVerifier<Signature> for VerifyingKey {
        /// Verify a signature over an already calculated SHA-256 hash.
        fn verify_prehash(&self, prehash: &[u8], signature: &Signature) -> Result<(), Error> {
            // Match ecdsa 0.16's bits2field: reject fewer than 128 bits,
            // left-pad short input, and truncate long input to the field size.
            if prehash.len() < 16 {
                return Err(Error::new());
            }
            let mut field_hash = [0; 32];
            if prehash.len() < 32 {
                field_hash[32 - prehash.len()..].copy_from_slice(prehash);
            } else {
                field_hash.copy_from_slice(&prehash[..32]);
            }
            let prehash = &field_hash;
            let signature = signature.to_bytes();
            let rc = unsafe {
                (self.table.psa_verify_hash)(
                    self.key,
                    ALG_ECDSA_SHA_256,
                    prehash.as_ptr(),
                    prehash.len(),
                    signature.as_ptr(),
                    signature.len(),
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(Error::new())
            }
        }
    }
    impl Drop for VerifyingKey {
        fn drop(&mut self) {
            unsafe {
                (self.table.psa_destroy_key)(self.key);
            }
        }
    }
}
