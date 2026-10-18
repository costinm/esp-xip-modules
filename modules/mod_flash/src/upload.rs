//! Incremental, transport-independent signed upload receiver.
//!
//! A caller feeds arbitrary TCP chunks, performs each returned flash action,
//! then resumes the session. The parser never owns a socket or a flash driver.
//! It retains one 4 KiB block, the signed header and the block-hash table.
use alloc::vec::Vec;
use crate::crypto::{Psa, Sha256};

use crate::wire::{
    self, target, Header, Reject, Transport, BLOCK, HEADER_SIZE, PUBKEY_SIZE, SIG_SIZE, TABLE_ENTRY,
};

/// How a package is authenticated. `Strict` is the signed upload (a key pinned on first use); the others are for development.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Auth {
    Strict,
    /// Signature and key are not checked (a package with zero key and signature); hashes still are. No rollback check.
    Unsigned,
    /// Signed by this key instead of the pinned key; nothing is pinned. No rollback check.
    Key([u8; PUBKEY_SIZE]),
}

pub(crate) trait Policy {
    fn auth(&self) -> Auth {
        Auth::Strict
    }
    fn cpu(&self) -> u8;
    fn pinned_key(&self) -> Option<[u8; PUBKEY_SIZE]>;
    fn require_pinned_key(&self) -> bool;
    fn min_version(&self, target: u8, name: &[u8]) -> Result<Option<u32>, Reject>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Transport,
    Pubkey,
    Signature,
    Header,
    Table,
    WaitOpen,
    Block,
    WaitWrite,
    Fin,
    WaitCommit,
    WaitHead,
    Done,
    ZeroFin,
}

/// A request for the caller. `Commit` must durably record the key and minimum
/// version before `committed` exposes block zero for writing.
pub(crate) enum Action<'a> {
    Open {
        target: u8,
        name: &'a [u8],
        image_size: u32,
        dry: bool,
    },
    Write {
        index: u32,
        bytes: &'a [u8],
    },
    Commit {
        target: u8,
        name: &'a [u8],
        version: u32,
        pubkey: &'a [u8; PUBKEY_SIZE],
    },
    RequestLoader {
        dry: bool,
    },
    Done {
        #[allow(dead_code)] // Used by the test-only blocking adapter.
        target: u8,
        image_size: u32,
        #[allow(dead_code)]
        dry: bool,
    },
}

pub(crate) struct Step<'a> {
    pub(crate) consumed: usize,
    pub(crate) action: Option<Action<'a>>,
}

pub(crate) struct Session {
    phase: Phase,
    buf: Vec<u8>,
    transport: Option<Transport>,
    pubkey: [u8; PUBKEY_SIZE],
    sig: [u8; SIG_SIZE],
    header: Option<Header>,
    table: Vec<u8>,
    head: Vec<u8>,
    block: Vec<u8>,
    crypto: Psa,
    whole: Sha256,
    offset: usize,
    index: u32,
}

impl Session {
    pub(crate) fn new(crypto: Psa) -> Result<Self, Reject> {
        Ok(Self {
            phase: Phase::Transport,
            buf: Vec::new(),
            transport: None,
            pubkey: [0; PUBKEY_SIZE],
            sig: [0; SIG_SIZE],
            header: None,
            table: Vec::new(),
            head: Vec::new(),
            block: Vec::new(),
            crypto,
            whole: crypto.sha256_stream().map_err(|_| wire::NO_MEMORY)?,
            offset: 0,
            index: 0,
        })
    }
    fn header(&self) -> &Header {
        self.header.as_ref().expect("header phase")
    }
    fn dry(&self) -> bool {
        self.transport.expect("transport phase").dry
    }
    fn wanted(&self) -> usize {
        match self.phase {
            Phase::Transport => wire::TRANSPORT_SIZE,
            Phase::Pubkey => PUBKEY_SIZE,
            Phase::Signature => SIG_SIZE,
            Phase::Header => HEADER_SIZE,
            Phase::Table => self.header().block_count as usize * TABLE_ENTRY,
            Phase::Block => (self.header().image_size as usize - self.offset).min(BLOCK),
            _ => 0,
        }
    }
    #[inline(always)]
    fn validate<P: Policy>(&self, p: &P, h: &Header) -> Result<(), Reject> {
        let auth = p.auth();
        if !h.magic_ok() {
            return Err(wire::BAD_SIGNATURE);
        }
        match auth {
            Auth::Unsigned => {}
            Auth::Key(key) => {
                if self.pubkey != key {
                    return Err(wire::KEY_MISMATCH);
                }
                self.crypto.verify_p256_sha256(&self.pubkey, &self.sig, &h.raw).map_err(|_| wire::BAD_SIGNATURE)?;
            }
            Auth::Strict => {
                if self.crypto.verify_p256_sha256(&self.pubkey, &self.sig, &h.raw).is_err() {
                    return Err(wire::BAD_SIGNATURE);
                }
                if p.pinned_key().is_some_and(|key| key != self.pubkey) {
                    return Err(wire::KEY_MISMATCH);
                }
                if p.require_pinned_key() && p.pinned_key().is_none() {
                    return Err(wire::NO_PINNED_KEY);
                }
            }
        }
        if h.format != wire::FORMAT || h.cpu != p.cpu() {
            return Err(wire::WRONG_TARGET);
        }
        if auth == Auth::Strict
            && p.min_version(h.target, h.name())?.is_some_and(|min| h.version < min)
        {
            return Err(wire::ROLLBACK);
        }
        if h.target == target::MAIN && h.image_size == 0 {
            return if h.block_count == 0 {
                Ok(())
            } else {
                Err(wire::BAD_LENGTH)
            };
        }
        if !matches!(
            h.target,
            target::MAIN | target::LOADER | target::STAGE2 | target::MODULE | target::FLASH
        ) {
            return Err(wire::WRONG_TARGET);
        }
        let size = h.image_size as usize;
        let min = if matches!(h.target, target::MODULE | target::FLASH) {
            1
        } else {
            BLOCK
        };
        if size < min
            || h.block_size as usize != BLOCK
            || h.block_count as usize != size.div_ceil(BLOCK)
        {
            return Err(wire::BAD_LENGTH);
        }
        Ok(())
    }

    /// Consume no more than one flash-action boundary. Feed `input[step.consumed..]`
    /// again after completing the action. A full packet can span many calls.
    #[inline(always)]
    pub(crate) fn feed<P: Policy>(&mut self, input: &[u8], policy: &P) -> Result<Step<'_>, Reject> {
        let mut used = 0;
        loop {
            let wanted = self.wanted();
            if wanted == 0 {
                return Ok(Step {
                    consumed: used,
                    action: None,
                });
            }
            let n = (wanted - self.buf.len()).min(input.len() - used);
            if n != 0 {
                self.buf.try_reserve(n).map_err(|_| wire::NO_MEMORY)?;
                self.buf.extend_from_slice(&input[used..used + n]);
                used += n;
            }
            if self.buf.len() != wanted {
                return Ok(Step {
                    consumed: used,
                    action: None,
                });
            }
            match self.phase {
                Phase::Transport => {
                    let t: &[u8; wire::TRANSPORT_SIZE] = self.buf.as_slice().try_into().unwrap();
                    let parsed = Transport::parse(t)?;
                    if parsed.reboot {
                        return Err(wire::UNSUPPORTED);
                    }
                    self.transport = Some(parsed);
                    self.phase = Phase::Pubkey;
                }
                Phase::Pubkey => {
                    self.pubkey.copy_from_slice(&self.buf);
                    self.phase = Phase::Signature;
                }
                Phase::Signature => {
                    self.sig.copy_from_slice(&self.buf);
                    self.phase = Phase::Header;
                }
                Phase::Header => {
                    let h = Header::parse(self.buf.as_slice().try_into().unwrap());
                    self.validate(policy, &h)?;
                    self.phase = if h.target == target::MAIN && h.image_size == 0 {
                        Phase::ZeroFin
                    } else {
                        Phase::Table
                    };
                    self.header = Some(h);
                }
                Phase::Table => {
                    if self.crypto.sha256(&self.buf).map_err(|_| wire::CRYPTO)?[..] != self.header().table_sha256 {
                        return Err(wire::BAD_TABLE);
                    }
                    self.table = core::mem::take(&mut self.buf);
                    // Allocate both image buffers before asking the caller to erase.
                    // Writes pause input, so the body buffer can be reused after
                    // every completed write rather than allocating a third block.
                    self.buf
                        .try_reserve_exact(BLOCK)
                        .map_err(|_| wire::NO_MEMORY)?;
                    self.head
                        .try_reserve_exact(BLOCK)
                        .map_err(|_| wire::NO_MEMORY)?;
                    self.phase = Phase::WaitOpen;
                    let h = self.header();
                    return Ok(Step {
                        consumed: used,
                        action: Some(Action::Open {
                            target: h.target,
                            name: h.name(),
                            image_size: h.image_size,
                            dry: self.dry(),
                        }),
                    });
                }
                Phase::Block => {
                    let (target, cpu, image_size) = {
                        let h = self.header();
                        (h.target, h.cpu, h.image_size as usize)
                    };
                    let want = &self.table[self.index as usize * TABLE_ENTRY
                        ..(self.index as usize + 1) * TABLE_ENTRY];
                    if self.crypto.sha256(&self.buf).map_err(|_| wire::CRYPTO)?[..TABLE_ENTRY] != *want {
                        return Err(wire::BAD_BLOCK);
                    }
                    if self.index == 0
                        && !matches!(target, target::MODULE | target::FLASH)
                        && (self.buf.len() < 14
                            || self.buf[0] != 0xe9
                            || u16::from_le_bytes([self.buf[12], self.buf[13]]) != u16::from(cpu))
                    {
                        return Err(wire::BAD_IMAGE_HEADER);
                    }
                    self.whole.update(&self.buf).map_err(|_| wire::CRYPTO)?;
                    self.offset += self.buf.len();
                    if self.index == 0 {
                        core::mem::swap(&mut self.head, &mut self.buf);
                        self.index = 1;
                        if self.offset == image_size {
                            self.phase = Phase::Fin;
                        }
                    } else if self.dry() {
                        self.buf.clear();
                        self.index += 1;
                        if self.offset == image_size {
                            self.phase = Phase::Fin;
                        }
                    } else {
                        self.block = core::mem::take(&mut self.buf);
                        self.phase = Phase::WaitWrite;
                        return Ok(Step {
                            consumed: used,
                            action: Some(Action::Write {
                                index: self.index,
                                bytes: &self.block,
                            }),
                        });
                    }
                }
                _ => unreachable!(),
            }
            self.buf.clear();
            if used == input.len() {
                return Ok(Step {
                    consumed: used,
                    action: None,
                });
            }
        }
    }

    #[inline(always)]
    pub(crate) fn opened(&mut self, ok: bool) -> Result<(), Reject> {
        if self.phase != Phase::WaitOpen {
            return Err(wire::UNSUPPORTED);
        }
        if !ok {
            return Err(wire::FLASH);
        }
        self.phase = Phase::Block;
        Ok(())
    }
    #[inline(always)]
    pub(crate) fn written(&mut self, ok: bool) -> Result<(), Reject> {
        if self.phase != Phase::WaitWrite && self.phase != Phase::WaitHead {
            return Err(wire::UNSUPPORTED);
        }
        if !ok {
            return Err(wire::FLASH);
        }
        if self.phase == Phase::WaitHead {
            self.phase = Phase::Done;
        } else {
            self.block.clear();
            core::mem::swap(&mut self.block, &mut self.buf);
            self.index += 1;
            self.phase = if self.offset == self.header().image_size as usize {
                Phase::Fin
            } else {
                Phase::Block
            };
        }
        Ok(())
    }
    /// Signal TCP EOF. Extra bytes after the image are rejected by the caller
    /// because `feed` consumes zero bytes in Fin/ZeroFin.
    #[inline(always)]
    pub(crate) fn eof(&mut self) -> Result<Action<'_>, Reject> {
        if self.phase == Phase::ZeroFin {
            self.phase = Phase::Done;
            return Ok(Action::RequestLoader { dry: self.dry() });
        }
        if self.phase != Phase::Fin {
            return Err(wire::SHORT_READ);
        }
        let (target, image_size, image_sha256) = {
            let h = self.header();
            (h.target, h.image_size, h.image_sha256)
        };
        if self.whole.finish().map_err(|_| wire::CRYPTO)?[..] != image_sha256 {
            return Err(wire::BAD_IMAGE_HASH);
        }
        if self.dry() {
            self.phase = Phase::Done;
            return Ok(Action::Done {
                target,
                image_size,
                dry: true,
            });
        }
        self.phase = Phase::WaitCommit;
        let h = self.header();
        Ok(Action::Commit {
            target: h.target,
            name: h.name(),
            version: h.version,
            pubkey: &self.pubkey,
        })
    }
    #[inline(always)]
    pub(crate) fn committed(&mut self, ok: bool) -> Result<Action<'_>, Reject> {
        if self.phase != Phase::WaitCommit {
            return Err(wire::UNSUPPORTED);
        }
        if !ok {
            return Err(wire::STATE_WRITE);
        }
        self.phase = Phase::WaitHead;
        Ok(Action::Write {
            index: 0,
            bytes: &self.head,
        })
    }
    #[cfg(test)]
    pub(crate) fn image_size(&self) -> Option<u32> {
        self.header.as_ref().map(|h| h.image_size)
    }
    pub(crate) fn done(&self) -> Option<(u8, u32)> {
        (self.phase == Phase::Done).then(|| (self.header().target, self.header().image_size))
    }
}
