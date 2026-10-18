//! The state log: two 4 KiB sectors used as an alternating append-only record log, at the end of the
//! loader partition. Same format as the C loader's `state_core.c` (so both can read and extend the
//! same flash): NOR flash turns bits 1 to 0 without an erase, so each record goes into the next
//! erased slot, and when a sector fills (or holds a torn record) the latest value of every record is
//! copied to the other sector, which is marked active before the old one is erased.
//!
//! Sector header (16 bytes): magic "WLS1", sequence, active mark `0xa5a5a5a5` (written last).
//! Record: `u32 {type, len, crc16}` (an erased word ends the log), then the payload padded to 4 bytes.

use crate::wire::{HEADER_SIZE, PUBKEY_SIZE};

pub(crate) const SECTOR: u32 = 4096;
/// Both sectors.
#[cfg(test)]
pub(crate) const SIZE: u32 = 2 * SECTOR;

pub(crate) const REC_PIN: u8 = 1;
pub(crate) const REC_MINVER: u8 = 2;
pub(crate) const REC_HEADER: u8 = 3;
pub(crate) const REC_QIO: u8 = 4;
pub(crate) const REC_MINVER_LOADER: u8 = 5;
pub(crate) const REC_MINVER_STAGE2: u8 = 6;
/// One module slot: `slot u8` + `u32` minimum version.
pub(crate) const REC_MINVER_MODULE: u8 = 7;
pub(crate) const MODULE_SLOTS: usize = 8;

const MAGIC: u32 = 0x3153_4c57; // "WLS1"
const ACTIVE: u32 = 0xa5a5_a5a5;
const SECTOR_HDR: u32 = 16;

/// Access to the 8 KiB area; offsets are relative to its start. Writes are 4-byte aligned and only
/// clear bits; `erase` takes a sector offset (0 or 4096).
pub(crate) trait FlashOps {
    fn read(&mut self, off: u32, buf: &mut [u8]) -> bool;
    fn write(&mut self, off: u32, data: &[u8]) -> bool;
    fn erase(&mut self, sector_off: u32) -> bool;
}

/// The latest value of every record.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) pin: Option<[u8; PUBKEY_SIZE]>,
    pub(crate) minver: Option<u32>,
    pub(crate) minver_loader: Option<u32>,
    pub(crate) minver_stage2: Option<u32>,
    pub(crate) minver_module: [Option<u32>; MODULE_SLOTS],
    pub(crate) header: Option<[u8; HEADER_SIZE]>,
    /// 0 none, 1 QIO ok, 2 DIO only.
    pub(crate) qio: u8,
}

impl View {
    fn apply(&mut self, ty: u8, d: &[u8]) {
        let u32_of =
            |d: &[u8]| (d.len() == 4).then(|| u32::from_le_bytes([d[0], d[1], d[2], d[3]]));
        match ty {
            REC_PIN if d.len() == PUBKEY_SIZE => self.pin = Some(d.try_into().unwrap()),
            REC_MINVER => self.minver = u32_of(d).or(self.minver),
            REC_MINVER_MODULE if d.len() == 5 && (d[0] as usize) < MODULE_SLOTS => {
                self.minver_module[d[0] as usize] =
                    Some(u32::from_le_bytes([d[1], d[2], d[3], d[4]]));
            }
            REC_HEADER if d.len() == HEADER_SIZE => self.header = Some(d.try_into().unwrap()),
            REC_QIO if d.len() == 1 => self.qio = d[0],
            REC_MINVER_LOADER => self.minver_loader = u32_of(d).or(self.minver_loader),
            REC_MINVER_STAGE2 => self.minver_stage2 = u32_of(d).or(self.minver_stage2),
            _ => {}
        }
    }
}

fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn record_word(ty: u8, len: u8, data: &[u8]) -> u32 {
    let crc = crc16(crc16(0xffff, &[ty, len]), data);
    ty as u32 | (len as u32) << 8 | (crc as u32) << 16
}

fn padded(len: usize) -> usize {
    (len + 3) & !3
}

pub(crate) struct StateLog<F: FlashOps> {
    ops: F,
    active: Option<u8>,
    seq: u32,
    used: u32,
    dirty: bool,
    pub(crate) view: View,
}

struct Scan {
    seq: u32,
    view: View,
    used: u32,
    dirty: bool,
}

fn scan_sector<F: FlashOps>(ops: &mut F, idx: u8) -> Option<Scan> {
    let base = idx as u32 * SECTOR;
    let mut hdr = [0u8; 12];
    if !ops.read(base, &mut hdr) {
        return None;
    }
    let (magic, seq, mark) = (
        u32::from_le_bytes(hdr[0..4].try_into().unwrap()),
        u32::from_le_bytes(hdr[4..8].try_into().unwrap()),
        u32::from_le_bytes(hdr[8..12].try_into().unwrap()),
    );
    if magic != MAGIC || mark != ACTIVE {
        return None;
    }
    let mut view = View::default();
    let (mut off, mut dirty) = (SECTOR_HDR, false);
    while off + 4 <= SECTOR {
        let mut w = [0u8; 4];
        if !ops.read(base + off, &mut w) {
            dirty = true;
            break;
        }
        let word = u32::from_le_bytes(w);
        if word == 0xffff_ffff {
            break;
        }
        let (ty, len) = (word as u8, (word >> 8) as u8);
        let next = off + 4 + padded(len as usize) as u32;
        let mut data = [0u8; 255 + 3];
        let n = padded(len as usize);
        if ty == 0
            || next > SECTOR
            || !ops.read(base + off + 4, &mut data[..n])
            || record_word(ty, len, &data[..len as usize]) != word
        {
            dirty = true;
            break;
        }
        view.apply(ty, &data[..len as usize]);
        off = next;
    }
    Some(Scan {
        seq,
        view,
        used: off,
        dirty,
    })
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl<F: FlashOps> StateLog<F> {
    /// Scan both sectors; blank flash gives an empty log.
    pub(crate) fn open(mut ops: F) -> Self {
        let mut best: Option<(u8, Scan)> = None;
        for idx in 0..2u8 {
            if let Some(s) = scan_sector(&mut ops, idx) {
                if best.as_ref().is_none_or(|(_, b)| newer(s.seq, b.seq)) {
                    best = Some((idx, s));
                }
            }
        }
        match best {
            Some((idx, s)) => StateLog {
                ops,
                active: Some(idx),
                seq: s.seq,
                used: s.used,
                dirty: s.dirty,
                view: s.view,
            },
            None => StateLog {
                ops,
                active: None,
                seq: 0,
                used: 0,
                dirty: false,
                view: View::default(),
            },
        }
    }

    pub(crate) fn ops(&mut self) -> &mut F {
        &mut self.ops
    }

    /// Whether a record with `payload` bytes still fits in the active sector (false: the next append compacts).
    #[cfg(test)]
    pub(crate) fn fits(&self, payload: usize) -> bool {
        self.active.is_some() && !self.dirty && self.used + 4 + padded(payload) as u32 <= SECTOR
    }

    /// Read both sectors back (a copy of the 8 KiB area), for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn flash_snapshot(&mut self) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec![0u8; SIZE as usize];
        let _ = self.ops.read(0, &mut v);
        v
    }

    fn write_record(&mut self, at: u32, ty: u8, data: &[u8]) -> bool {
        let mut buf = [0xffu8; 4 + 255 + 3];
        buf[0..4].copy_from_slice(&record_word(ty, data.len() as u8, data).to_le_bytes());
        buf[4..4 + data.len()].copy_from_slice(data);
        self.ops.write(at, &buf[..4 + padded(data.len())])
    }

    /// Start the other sector as the active log holding the current view (which already includes the
    /// record being appended); the old sector is erased only after the new one is marked active.
    fn compact(&mut self) -> bool {
        let to = match self.active {
            None => 0u8,
            Some(a) => 1 - a,
        };
        let base = to as u32 * SECTOR;
        let seq = if self.active.is_none() {
            1
        } else {
            self.seq.wrapping_add(1)
        };
        if !self.ops.erase(base) {
            return false;
        }
        let mut hdr = [0u8; 8];
        hdr[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[4..8].copy_from_slice(&seq.to_le_bytes());
        if !self.ops.write(base, &hdr) {
            return false;
        }
        let v = self.view.clone();
        let mut off = SECTOR_HDR;
        let put = |log: &mut Self, ty: u8, data: &[u8], off: &mut u32| -> bool {
            if !log.write_record(base + *off, ty, data) {
                return false;
            }
            *off += 4 + padded(data.len()) as u32;
            true
        };
        if let Some(p) = &v.pin {
            if !put(self, REC_PIN, p, &mut off) {
                return false;
            }
        }
        for (ty, val) in [
            (REC_MINVER, v.minver),
            (REC_MINVER_LOADER, v.minver_loader),
            (REC_MINVER_STAGE2, v.minver_stage2),
        ] {
            if let Some(x) = val {
                if !put(self, ty, &x.to_le_bytes(), &mut off) {
                    return false;
                }
            }
        }
        for (slot, val) in v.minver_module.iter().enumerate() {
            if let Some(x) = val {
                let mut rec = [0u8; 5];
                rec[0] = slot as u8;
                rec[1..].copy_from_slice(&x.to_le_bytes());
                if !put(self, REC_MINVER_MODULE, &rec, &mut off) {
                    return false;
                }
            }
        }
        if let Some(h) = &v.header {
            if !put(self, REC_HEADER, h, &mut off) {
                return false;
            }
        }
        if v.qio != 0 && !put(self, REC_QIO, &[v.qio], &mut off) {
            return false;
        }
        if !self.ops.write(base + 8, &ACTIVE.to_le_bytes()) {
            return false;
        }
        let old = self.active;
        self.active = Some(to);
        self.seq = seq;
        self.used = off;
        self.dirty = false;
        if let Some(o) = old {
            let _ = self.ops.erase(o as u32 * SECTOR);
        }
        true
    }

    /// Append one record (compacting into the other sector when needed). False on a flash failure;
    /// the in-memory view is then left as it was.
    pub(crate) fn append(&mut self, ty: u8, data: &[u8]) -> bool {
        if ty == 0 || data.len() > 255 {
            return false;
        }
        let before = self.view.clone();
        self.view.apply(ty, data);
        let need = 4 + padded(data.len()) as u32;
        let ok = match self.active {
            Some(a) if !self.dirty && self.used + need <= SECTOR => {
                let at = a as u32 * SECTOR + self.used;
                if self.write_record(at, ty, data) {
                    self.used += need;
                    true
                } else {
                    self.dirty = true; // the slot may be partly programmed
                    false
                }
            }
            _ => self.compact(),
        };
        if !ok {
            self.view = before;
        }
        ok
    }

    pub(crate) fn append_u32(&mut self, ty: u8, v: u32) -> bool {
        self.append(ty, &v.to_le_bytes())
    }

    /// Record the minimum version of one module slot.
    pub(crate) fn append_module_version(&mut self, slot: u8, v: u32) -> bool {
        let mut rec = [0u8; 5];
        rec[0] = slot;
        rec[1..].copy_from_slice(&v.to_le_bytes());
        self.append(REC_MINVER_MODULE, &rec)
    }
}
