//! Private, fallible upload crypto helpers; not a public crypto API. The module links no crypto: the runtime's `crypto` table does the work
//! on packets (`xip_abi::CryptoClient`).
use xip_abi::{CryptoClient, Hw};

#[derive(Clone, Copy)]
pub(crate) struct Psa {
    client: CryptoClient,
}
impl Psa {
    /// The runtime's `crypto` component.
    pub fn from_host(hw: Hw) -> Option<Self> {
        CryptoClient::from_host(hw).map(|client| Self { client })
    }
    pub fn sha256(self, input: &[u8]) -> Result<[u8; 32], i32> {
        self.client.sha256(input)
    }
    pub fn sha256_stream(self) -> Result<Sha256, i32> {
        self.client.sha256_stream().map(Sha256)
    }
    /// P-256 verification: uncompressed SEC1 point, raw r || s signature.
    pub fn verify_p256_sha256(self, public_key: &[u8; 65], signature: &[u8; 64], message: &[u8]) -> Result<(), i32> {
        self.client.verify_p256_sha256(public_key, signature, message)
    }
}

/// Owned multipart hash. Finish releases the runtime's state; Drop also aborts on disconnect, failed uploads and module stop.
pub(crate) struct Sha256(xip_abi::Sha256);
impl Sha256 {
    pub fn update(&mut self, input: &[u8]) -> Result<(), i32> {
        self.0.update(input)
    }
    pub fn finish(&mut self) -> Result<[u8; 32], i32> {
        self.0.finish()
    }
}
