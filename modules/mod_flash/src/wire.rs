//! Wire format: all integers little-endian.
//!
//! ```text
//! transport header (16): "WLU1" | format u8 = 1 | flags u8 | op u8 | rsv u8 | 8 zero bytes
//! public key (65) | signature (64) | signed header (216) | block table (8 * block_count) | image
//! ```

#[cfg(test)]
use alloc::vec::Vec;
#[cfg(test)]
use sha2::{Digest, Sha256};

pub(crate) const PORT: u16 = 5227;
pub(crate) const FORMAT: u8 = 1;
pub(crate) const TRANSPORT_SIZE: usize = 16;
pub(crate) const PUBKEY_SIZE: usize = 65;

pub(crate) const SIG_SIZE: usize = 64;
pub(crate) const HEADER_SIZE: usize = 216;
pub(crate) const BLOCK: usize = 4096;
pub(crate) const TABLE_ENTRY: usize = 8;
/// Public key + signature + signed header.
#[cfg(test)]
pub(crate) const PACKAGE_FIXED: usize = PUBKEY_SIZE + SIG_SIZE + HEADER_SIZE;
pub(crate) const HEADER_MAGIC: u32 = 0x3148_4c57; // "WLH1"

/// Signed header targets. Sinks decide which named targets they implement;
/// the optional mod_flash implements all of these, including raw addresses.
pub(crate) mod target {
    pub(crate) const MAIN: u8 = 1;
    pub(crate) const LOADER: u8 = 2;
    pub(crate) const STAGE2: u8 = 3;
    /// A module slot in the `data` partition; the module name is in the header's name field.
    pub(crate) const MODULE: u8 = 4;
    /// Arbitrary signed flash bytes; name is a hexadecimal physical address.
    pub(crate) const FLASH: u8 = 5;
}

/// `esp_chip_id_t`.
#[cfg(test)]
pub(crate) mod cpu {
    #[allow(dead_code)]
    pub(crate) const ESP32: u8 = 0;
    #[allow(dead_code)]
    pub(crate) const ESP32S3: u8 = 9;
    pub(crate) const ESP32C6: u8 = 13;
}

// Flat PIC images cannot contain absolute string pointers in static error
// records. The module build keeps the same wire reasons inline instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reason {
    bytes: [u8; 32],
    len: u8,
}
impl Reason {
    const fn new(reason: &str) -> Self {
        let mut bytes = [0; 32];
        let input = reason.as_bytes();
        let mut i = 0;
        assert!(input.len() <= 32);
        while i < input.len() {
            bytes[i] = input[i];
            i += 1;
        }
        Self {
            bytes,
            len: input.len() as u8,
        }
    }
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}
impl core::fmt::Display for Reason {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str(core::str::from_utf8(self.as_bytes()).map_err(|_| core::fmt::Error)?)
    }
}

/// A protocol error: `ERR <code> <reason>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reject {
    pub(crate) code: u8,
    pub(crate) reason: Reason,
}

impl Reject {
    pub(crate) const fn new(code: u8, reason: &'static str) -> Self {
        Reject {
            code,
            reason: Reason::new(reason),
        }
    }
}

pub(crate) const BAD_HEADER: Reject = Reject::new(1, "bad-header");
pub(crate) const UNSUPPORTED: Reject = Reject::new(2, "unsupported-header");
pub(crate) const BAD_LENGTH: Reject = Reject::new(3, "bad-length");
pub(crate) const BAD_IMAGE_HEADER: Reject = Reject::new(4, "bad-image-header");
pub(crate) const FLASH: Reject = Reject::new(5, "flash-write");
pub(crate) const SHORT_READ: Reject = Reject::new(6, "short-read-or-timeout");
pub(crate) const BAD_SIGNATURE: Reject = Reject::new(7, "bad-signature");
pub(crate) const KEY_MISMATCH: Reject = Reject::new(8, "key-mismatch");
pub(crate) const ROLLBACK: Reject = Reject::new(9, "rollback");
pub(crate) const WRONG_TARGET: Reject = Reject::new(10, "wrong-chip-or-target");
pub(crate) const NO_MEMORY: Reject = Reject::new(11, "no-memory");
pub(crate) const BAD_TABLE: Reject = Reject::new(12, "bad-block-table");
pub(crate) const BAD_BLOCK: Reject = Reject::new(13, "bad-block");
pub(crate) const BAD_IMAGE_HASH: Reject = Reject::new(14, "bad-image-hash");
#[cfg(test)]
pub(crate) const EXPECTED_FIN: Reject = Reject::new(6, "expected-fin");
pub(crate) const CRYPTO: Reject = Reject::new(5, "crypto");
pub(crate) const STATE_WRITE: Reject = Reject::new(5, "state-write");
pub(crate) const NO_PINNED_KEY: Reject = Reject::new(8, "no-pinned-key");

/// The 16-byte header that starts every connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Transport {
    /// Verify everything, erase and write nothing.
    pub(crate) dry: bool,
    /// No package follows: restart into Main.
    pub(crate) reboot: bool,
}

impl Transport {
    #[inline(always)]
    pub(crate) fn parse(h: &[u8; TRANSPORT_SIZE]) -> Result<Self, Reject> {
        if &h[0..4] != b"WLU1" || h[4] != FORMAT {
            return Err(BAD_HEADER);
        }
        if h[5] & !2 != 0 || h[7] != 0 || (h[6] != 0 && h[6] != 255) {
            return Err(UNSUPPORTED);
        }
        Ok(Transport {
            dry: h[5] & 2 != 0,
            reboot: h[6] == 255,
        })
    }

    #[cfg(test)]
    pub(crate) fn encode(dry: bool, reboot: bool) -> [u8; TRANSPORT_SIZE] {
        let mut h = [0u8; TRANSPORT_SIZE];
        h[0..4].copy_from_slice(b"WLU1");
        h[4] = FORMAT;
        h[5] = if dry { 2 } else { 0 };
        h[6] = if reboot { 255 } else { 0 };
        h
    }
}

/// The signed 216-byte header.
#[derive(Clone, Debug)]
pub(crate) struct Header {
    pub(crate) raw: [u8; HEADER_SIZE],
    pub(crate) format: u8,
    pub(crate) cpu: u8,
    pub(crate) target: u8,

    pub(crate) version: u32,
    pub(crate) image_size: u32,
    pub(crate) block_size: u16,
    pub(crate) block_count: u16,
    pub(crate) image_sha256: [u8; 32],
    pub(crate) table_sha256: [u8; 32],
    name_len: u8,
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Header {
    #[inline(always)]
    pub(crate) fn parse(raw: &[u8; HEADER_SIZE]) -> Self {
        let mut image_sha256 = [0u8; 32];
        let mut table_sha256 = [0u8; 32];
        image_sha256.copy_from_slice(&raw[20..52]);
        table_sha256.copy_from_slice(&raw[52..84]);
        Header {
            raw: *raw,
            format: raw[4],
            cpu: raw[5],
            target: raw[6],
            version: le32(&raw[8..12]),
            image_size: le32(&raw[12..16]),
            block_size: u16::from_le_bytes([raw[16], raw[17]]),
            block_count: u16::from_le_bytes([raw[18], raw[19]]),
            image_sha256,
            table_sha256,
            name_len: raw[84],
        }
    }

    pub(crate) fn magic_ok(&self) -> bool {
        le32(&self.raw[0..4]) == HEADER_MAGIC
    }

    /// The name field: the download host for a Main package (informational), the module name for a
    /// module package.
    pub(crate) fn name(&self) -> &[u8] {
        let n = (self.name_len as usize).min(128);
        &self.raw[88..88 + n]
    }

    /// Build a header (a signing tool or a test). `name` is at most 128 bytes.
    #[cfg(test)]
    pub(crate) fn build(
        cpu: u8,
        target: u8,
        version: u32,
        image: &[u8],
        name: &[u8],
    ) -> [u8; HEADER_SIZE] {
        let table = block_table(image);
        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        h[4] = FORMAT;
        h[5] = cpu;
        h[6] = target;
        h[8..12].copy_from_slice(&version.to_le_bytes());
        h[12..16].copy_from_slice(&(image.len() as u32).to_le_bytes());
        h[16..18].copy_from_slice(&(BLOCK as u16).to_le_bytes());
        h[18..20].copy_from_slice(&((table.len() / TABLE_ENTRY) as u16).to_le_bytes());
        h[20..52].copy_from_slice(&Sha256::digest(image));
        h[52..84].copy_from_slice(&Sha256::digest(&table));
        let n = name.len().min(128);
        h[84] = n as u8;
        h[88..88 + n].copy_from_slice(&name[..n]);
        h
    }
}

/// Truncated SHA-256 of every 4 KiB block of `image` (the last block may be short).
#[cfg(test)]
pub(crate) fn block_table(image: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(image.len().div_ceil(BLOCK) * TABLE_ENTRY);
    for block in image.chunks(BLOCK) {
        t.extend_from_slice(&Sha256::digest(block)[..TABLE_ENTRY]);
    }
    t
}

/// ECDSA P-256 over SHA-256(`msg`), signature as `r || s`, key as an uncompressed point.
#[cfg(test)]
pub(crate) fn verify_signature(
    pubkey: &[u8; PUBKEY_SIZE],
    sig: &[u8; SIG_SIZE],
    msg: &[u8],
) -> bool {
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    let Ok(key) = VerifyingKey::from_sec1_bytes(pubkey) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    key.verify(msg, &sig).is_ok()
}

/// A parsed package held in memory (a host sending it, or a test).
#[cfg(test)]
pub(crate) struct Package<'a> {
    pub(crate) pubkey: &'a [u8; PUBKEY_SIZE],
    pub(crate) sig: &'a [u8; SIG_SIZE],
    pub(crate) header: Header,
    pub(crate) table: &'a [u8],
    pub(crate) image: &'a [u8],
}

#[cfg(test)]
impl<'a> Package<'a> {
    #[inline(always)]
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, Reject> {
        if bytes.len() < PACKAGE_FIXED {
            return Err(BAD_LENGTH);
        }
        let header = Header::parse(
            bytes[PUBKEY_SIZE + SIG_SIZE..PACKAGE_FIXED]
                .try_into()
                .unwrap(),
        );
        let table_end = PACKAGE_FIXED + header.block_count as usize * TABLE_ENTRY;
        if bytes.len() < table_end {
            return Err(BAD_LENGTH);
        }
        Ok(Package {
            pubkey: bytes[..PUBKEY_SIZE].try_into().unwrap(),
            sig: bytes[PUBKEY_SIZE..PUBKEY_SIZE + SIG_SIZE]
                .try_into()
                .unwrap(),
            header,
            table: &bytes[PACKAGE_FIXED..table_end],
            image: &bytes[table_end..],
        })
    }

    /// The checks a flasher makes before sending, in the device's order: None when valid.
    pub(crate) fn verify(&self) -> Option<Reject> {
        let h = &self.header;
        if !h.magic_ok() || h.format != FORMAT {
            return Some(BAD_HEADER);
        }
        if !verify_signature(self.pubkey, self.sig, &h.raw) {
            return Some(BAD_SIGNATURE);
        }
        if h.block_size as usize != BLOCK
            || h.block_count as usize != (h.image_size as usize).div_ceil(BLOCK)
            || self.image.len() != h.image_size as usize
        {
            return Some(BAD_LENGTH);
        }
        if Sha256::digest(self.table)[..] != h.table_sha256 {
            return Some(BAD_TABLE);
        }
        for (i, block) in self.image.chunks(BLOCK).enumerate() {
            if Sha256::digest(block)[..TABLE_ENTRY]
                != self.table[i * TABLE_ENTRY..(i + 1) * TABLE_ENTRY]
            {
                return Some(BAD_BLOCK);
            }
        }
        if Sha256::digest(self.image)[..] != h.image_sha256 {
            return Some(BAD_IMAGE_HASH);
        }
        None
    }
}
