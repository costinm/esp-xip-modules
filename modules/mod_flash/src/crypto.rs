//! Private, fallible upload crypto helpers; not a public crypto API.
use xip_abi::{psa::*, Hw};

#[derive(Clone, Copy)]
pub(crate) struct Psa {
    table: &'static PsaTable,
}
impl Psa {
    /// The `psa` table is host-owned for the firmware lifetime.
    pub fn from_host(hw: Hw) -> Option<Self> {
        let name = core::ffi::CStr::from_bytes_with_nul(b"psa\0").unwrap();
        unsafe { Self::from_table(hw.get_component(name).cast()) }
    }
    /// # Safety
    /// `table` must be NULL or a valid host-owned native PSA table with its
    /// version/size prefix readable and u32-aligned, matching context layouts, and all present
    /// function pointers callable for the firmware lifetime.
    pub unsafe fn from_table(table: *const PsaTable) -> Option<Self> {
        Some(Self {
            table: PsaTable::from_table(table)?,
        })
    }
    pub fn sha256(self, input: &[u8]) -> Result<[u8; 32], i32> {
        let mut out = [0; 32];
        let mut len = 0;
        let rc = unsafe {
            (self.table.psa_hash_compute)(
                ALG_SHA_256,
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut len,
            )
        };
        digest_result(rc, len, out)
    }
    pub fn sha256_stream(self) -> Result<Sha256, i32> {
        let mut hash = Sha256 {
            psa: self,
            operation: HashOperation::default(),
        };
        let rc = unsafe { (self.table.psa_hash_setup)(&mut hash.operation, ALG_SHA_256) };
        if rc != 0 {
            return Err(rc);
        } // Drop aborts even a partially initialized operation.
        Ok(hash)
    }
    /// Native P-256 verification: uncompressed SEC1 point, raw r || s signature.
    /// Imported keys are destroyed on both success and verification failure.
    pub fn verify_p256_sha256(
        self,
        public_key: &[u8; 65],
        signature: &[u8; 64],
        message: &[u8],
    ) -> Result<(), i32> {
        let hash = self.sha256(message)?;
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
        let mut key = 0;
        let rc = unsafe {
            (self.table.psa_import_key)(
                &attributes,
                public_key.as_ptr(),
                public_key.len(),
                &mut key,
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        let rc = unsafe {
            (self.table.psa_verify_hash)(
                key,
                ALG_ECDSA_SHA_256,
                hash.as_ptr(),
                hash.len(),
                signature.as_ptr(),
                signature.len(),
            )
        };
        let cleanup = unsafe { (self.table.psa_destroy_key)(key) };
        if rc != 0 {
            Err(rc)
        } else if cleanup != 0 {
            Err(cleanup)
        } else {
            Ok(())
        }
    }
}

fn digest_result(rc: i32, len: usize, out: [u8; 32]) -> Result<[u8; 32], i32> {
    if rc != 0 {
        Err(rc)
    } else if len != out.len() {
        Err(ERROR_CORRUPTION_DETECTED)
    } else {
        Ok(out)
    }
}
/// Owned native hash operation. Finish releases its native state; Drop also
/// aborts on disconnect, failed uploads and module stop.
pub(crate) struct Sha256 {
    psa: Psa,
    operation: HashOperation,
}
impl Sha256 {
    pub fn update(&mut self, input: &[u8]) -> Result<(), i32> {
        let rc = unsafe {
            (self.psa.table.psa_hash_update)(&mut self.operation, input.as_ptr(), input.len())
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(rc)
        }
    }
    pub fn finish(&mut self) -> Result<[u8; 32], i32> {
        let mut out = [0; 32];
        let mut len = 0;
        let rc = unsafe {
            (self.psa.table.psa_hash_finish)(
                &mut self.operation,
                out.as_mut_ptr(),
                out.len(),
                &mut len,
            )
        };
        digest_result(rc, len, out)
    }
}
impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            (self.psa.table.psa_hash_abort)(&mut self.operation);
        }
    }
}
