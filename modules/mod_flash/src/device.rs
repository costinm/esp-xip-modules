//! Test-only blocking adapter, retained for interoperability coverage.
//! Serving one connection on a device: verify and install a signed package, or answer a zero-length
//! Main manifest. The firmware provides the stream (an accepted TCP socket with timeouts), the
//! flash destinations ([`Platform`]) and the state sectors ([`FlashOps`]); this module is the same
//! sequence of checks as the C loader's `upload.c`:
//!
//! 1. before anything is erased: signature, pinned key, minimum version, chip, target, sizes, and the
//!    block table against its hash;
//! 2. per 4 KiB block: its hash against the table, then written (block 0 is held back);
//! 3. the whole-image hash, then the client's FIN;
//! 4. commit: state records durably saved, then block 0 last. An interrupted install leaves block 0 unwritten, so
//!    the image cannot boot.

use std::io::{Read, Write};
use std::time::Instant;

use crate::state::{FlashOps, StateLog, REC_MINVER_LOADER, REC_MINVER_STAGE2, REC_PIN};
use crate::upload::{Action, Policy, Session};
use crate::wire::{self, target, Reject};

/// Where one image goes.
pub(crate) trait Sink {
    /// Once, before the first block, with the image length.
    fn begin(&mut self, len: u32) -> Result<(), ()>;
    /// A 4 KiB block (the last may be shorter). Blocks arrive in order except block 0, which is
    /// written last.
    fn write_block(&mut self, index: u32, bytes: &[u8]) -> Result<(), ()>;
    fn finish(&mut self) -> Result<(), ()>;
}

pub(crate) trait Platform {
    type Sink: Sink;
    /// `esp_chip_id_t` of the running chip ([`wire::cpu`]).
    fn cpu(&self) -> u8;
    /// Open the destination for a header that was already verified. `target` is LOADER, STAGE2 or
    /// MODULE (then `name` is the module name); refuse with a [`Reject`] if this device does not take
    /// it, the name is unknown, or `image_size` does not fit. With `dry` nothing will be written.
    fn open(
        &mut self,
        target: u8,
        name: &[u8],
        image_size: u32,
        dry: bool,
    ) -> Result<Self::Sink, Reject>;
    /// The slot number (0..[`crate::state::MODULE_SLOTS`]) of a module name, under which its minimum
    /// version is kept; None for an unknown name.
    fn module_slot(&self, name: &[u8]) -> Option<u8>;
}

/// What the caller does after [`Device::serve`] returned and the connection is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing changed (an error was reported, or a dry run).
    None,
    /// An image was written and committed.
    Installed { target: u8 },
    /// A zero-length Main manifest was accepted: set the RTC handoff to [`crate::rtc::HANDOFF_LOADER`]
    /// and restart (the reply was already sent; let it drain first).
    RequestLoader,
}

pub(crate) struct Device<P: Platform, F: FlashOps> {
    pub(crate) platform: P,
    pub(crate) state: StateLog<F>,
    require_pin: bool,
}

struct DevicePolicy<'a, P: Platform, F: FlashOps>(&'a Device<P, F>);
impl<P: Platform, F: FlashOps> Policy for DevicePolicy<'_, P, F> {
    fn cpu(&self) -> u8 {
        self.0.platform.cpu()
    }
    fn pinned_key(&self) -> Option<[u8; wire::PUBKEY_SIZE]> {
        self.0.state.view.pin
    }
    fn require_pinned_key(&self) -> bool {
        self.0.require_pin
    }
    fn min_version(&self, target: u8, name: &[u8]) -> Result<Option<u32>, Reject> {
        let slot = if target == target::MODULE {
            Some(
                self.0
                    .platform
                    .module_slot(name)
                    .ok_or(wire::WRONG_TARGET)?,
            )
        } else {
            None
        };
        Ok(match target {
            target::MAIN => self.0.state.view.minver,
            target::LOADER => self.0.state.view.minver_loader,
            target::STAGE2 => self.0.state.view.minver_stage2,
            target::MODULE => slot.and_then(|s| self.0.state.view.minver_module[s as usize]),
            _ => None,
        })
    }
}

impl<P: Platform, F: FlashOps> Device<P, F> {
    pub(crate) fn new(platform: P, flash: F) -> Self {
        Device {
            platform,
            state: StateLog::open(flash),
            require_pin: false,
        }
    }

    pub(crate) fn require_pinned_key(mut self) -> Self {
        self.require_pin = true;
        self
    }

    pub(crate) fn serve<S: Read + Write>(&mut self, s: &mut S) -> Outcome {
        match self.upload(s) {
            Ok(outcome) => outcome,
            Err(r) => {
                let _ = writeln!(s, "ERR {} {}", r.code, r.reason);
                Outcome::None
            }
        }
    }

    /// Blocking std::io adapter over the no_std incremental upload session.
    /// The application can instead drive Session from its shared socket poller.
    fn upload<S: Read + Write>(&mut self, s: &mut S) -> Result<Outcome, Reject> {
        let started = Instant::now();
        let mut session = Session::new(crate::psa_test::psa()).unwrap();
        let mut sink: Option<P::Sink> = None;
        let mut buf = [0u8; 1000];
        let mut offset = 0usize;
        let mut len = 0usize;
        loop {
            if offset == len {
                len = s.read(&mut buf).map_err(|_| wire::SHORT_READ)?;
                offset = 0;
                if len == 0 {
                    let action = session.eof()?;
                    return match action {
                        Action::RequestLoader { dry } => {
                            let _ =
                                writeln!(s, "OK 0 request-loader{}", if dry { " dry" } else { "" });
                            Ok(if dry {
                                Outcome::None
                            } else {
                                Outcome::RequestLoader
                            })
                        }
                        Action::Done {
                            target,
                            image_size,
                            dry,
                        } => {
                            let _ = writeln!(
                                s,
                                "OK {} ms={}{}",
                                image_size,
                                started.elapsed().as_millis(),
                                if dry { " dry" } else { "" }
                            );
                            Ok(if dry {
                                Outcome::None
                            } else {
                                Outcome::Installed { target }
                            })
                        }
                        Action::Commit {
                            target,
                            name,
                            version,
                            pubkey,
                        } => {
                            let slot = if target == target::MODULE {
                                self.platform.module_slot(name)
                            } else {
                                None
                            };
                            let saved = (self.state.view.pin.is_some()
                                || self.state.append(REC_PIN, pubkey))
                                && match target {
                                    target::LOADER => {
                                        self.state.append_u32(REC_MINVER_LOADER, version)
                                    }
                                    target::STAGE2 => {
                                        self.state.append_u32(REC_MINVER_STAGE2, version)
                                    }
                                    _ => slot.is_none_or(|s| {
                                        self.state.append_module_version(s, version)
                                    }),
                                };
                            let head = session.committed(saved)?;
                            if let Action::Write { index: 0, bytes } = head {
                                sink.as_mut()
                                    .ok_or(wire::FLASH)?
                                    .write_block(0, bytes)
                                    .map_err(|_| wire::FLASH)?;
                                session.written(true)?;
                                sink.as_mut()
                                    .ok_or(wire::FLASH)?
                                    .finish()
                                    .map_err(|_| wire::FLASH)?;
                                let (_, size) = session.done().ok_or(wire::FLASH)?;
                                let _ =
                                    writeln!(s, "OK {} ms={}", size, started.elapsed().as_millis());
                                Ok(Outcome::Installed { target })
                            } else {
                                Err(wire::FLASH)
                            }
                        }
                        _ => Err(wire::SHORT_READ),
                    };
                }
            }
            let step = session.feed(&buf[offset..len], &DevicePolicy(self))?;
            offset += step.consumed;
            match step.action {
                Some(Action::Open {
                    target,
                    name,
                    image_size,
                    dry,
                }) => {
                    let mut opened = self.platform.open(target, name, image_size, dry)?;
                    if !dry {
                        opened.begin(image_size).map_err(|_| wire::FLASH)?;
                    }
                    sink = Some(opened);
                    session.opened(true)?;
                }
                Some(Action::Write { index, bytes }) => {
                    sink.as_mut()
                        .ok_or(wire::FLASH)?
                        .write_block(index, bytes)
                        .map_err(|_| wire::FLASH)?;
                    let written = index as usize * wire::BLOCK + bytes.len();
                    session.written(true)?;
                    let image_size = session.image_size().unwrap_or(0);
                    if written % (64 * 1024) == 0
                        || (image_size != 0 && written == image_size as usize)
                    {
                        let _ =
                            writeln!(s, "P {} {}", written, written.saturating_sub(wire::BLOCK));
                    }
                }
                Some(_) => return Err(wire::UNSUPPORTED),
                None => {
                    if step.consumed == 0 {
                        return Err(wire::EXPECTED_FIN);
                    }
                }
            }
        }
    }
}
