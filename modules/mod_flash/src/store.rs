//! Filesystem format reader. Bodies remain in flash; scans use fixed-size headers.
use xip_abi::{Hw, ERR_ARG, ERR_IO, ERR_NOTFOUND, ERR_SIZE, ERR_UNSUPPORTED};
pub const PAGE: u32 = 65536;
pub const FRONT: u32 = 0x180000;
pub const MAX_FILE: u32 = 65504;
pub const NOSPACE: i32 = -20;
pub const VERSION: i32 = -21;
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub base: u32,
    pub size: u32,
    pub cursor: u32,
    pub pending: Option<(u32, u32)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File {
    pub name: [u8; 8],
    pub version: u32,
    pub size: u32,
    pub at: u32,
    pub kind: u8,
    pub cache: bool,
    pub live: bool,
}
#[derive(Clone, Copy)]
pub struct Area {
    pub start: u32,
    pub end: u32,
    pub kind: u8,
    pub bytes: u32,
}
pub fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
pub fn align(n: u32) -> u32 {
    (n + 3) & !3
}
impl Layout {
    pub fn discover(hw: Hw) -> Result<Self, i32> {
        for i in 0..96 {
            let mut e = [0; 32];
            if hw.flash_read(0x8000, i * 32, &mut e) != 32 {
                return Err(ERR_IO);
            }
            if e[..2] == [0xff, 0xff] || e[..2] == [0xeb, 0xeb] {
                break;
            }
            if e[..2] != [0xaa, 0x50] {
                return Err(ERR_ARG);
            }
            if &e[12..16] == b"xip\0" {
                let base = u32le(&e[4..]);
                let size = u32le(&e[8..]);
                if base % PAGE != 0
                    || size % PAGE != 0
                    || size <= FRONT
                    || base.checked_add(size).is_none()
                {
                    return Err(ERR_ARG);
                }
                return Ok(Self {
                    base,
                    size,
                    cursor: size,
                    pending: None,
                });
            }
        }
        Err(ERR_NOTFOUND)
    }
    pub fn read(self, hw: Hw, at: u32, b: &mut [u8]) -> Result<(), i32> {
        if at > self.size || b.len() > (self.size - at) as usize {
            return Err(ERR_ARG);
        }
        if hw.flash_read(self.base, at, b) != b.len() as i32 {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
    pub fn image_size(self, hw: Hw, at: u32) -> Result<Option<u32>, i32> {
        let room = (self.size - at).min(255 * PAGE);
        let mut h = [0; 36];
        self.read(hw, at, &mut h)?;
        if h[0] != 0xe9 || h[1] == 0 || h[1] > 16 || u32le(&h[32..]) != 0xabcd5432 {
            return Ok(None);
        }
        let mut n = 24;
        for _ in 0..h[1] {
            if n > room - 8 {
                return Ok(None);
            }
            let mut s = [0; 8];
            self.read(hw, at + n, &mut s)?;
            let len = u32le(&s[4..]);
            if len > room - n - 8 {
                return Ok(None);
            }
            n += 8 + len;
        }
        n = (n | 15) + 1;
        if h[23] != 0 {
            n += 32;
        }
        Ok((n <= room).then_some(n))
    }
    #[inline(never)]
    pub fn walk(self, hw: Hw, mut visit: impl FnMut(Area) -> Result<(), i32>) -> Result<(), i32> {
        let mut at = 0;
        while at < self.size {
            if self
                .pending
                .is_some_and(|(start, span)| at >= start && at < start + span)
            {
                at += PAGE;
                continue;
            }
            let mut h = [0; 16];
            self.read(hw, at, &mut h)?;
            let len = match h[0] {
                b'D' | b'X' => {
                    if u32le(&h[4..]) == PAGE {
                        PAGE
                    } else {
                        0
                    }
                }
                b'M' | 0x4f => {
                    let n = u32le(&h[4..]);
                    if n > 64 {
                        n
                    } else {
                        0
                    }
                }
                0 => u32le(&h[4..]),
                0xe9 => self.image_size(hw, at)?.unwrap_or(0),
                _ => 0,
            };
            let valid = len > 0 && len <= self.size - at && len.div_ceil(PAGE) <= 255;
            let span = if valid {
                len.div_ceil(PAGE) * PAGE
            } else {
                PAGE
            };
            if valid {
                visit(Area {
                    start: at,
                    end: at + span,
                    kind: h[0],
                    bytes: if matches!(h[0], b'D' | b'X') {
                        16
                    } else {
                        align(len)
                    },
                })?;
            }
            at += span;
        }
        Ok(())
    }
    #[inline(never)]
    pub fn areas(self, hw: Hw, mut visit: impl FnMut(Area) -> Result<(), i32>) -> Result<(), i32> {
        self.walk(hw, |a| {
            if matches!(a.kind, b'D' | b'X' | b'M' | 0xe9) {
                visit(a)?;
            }
            Ok(())
        })
    }
    #[inline(never)]
    pub fn occupied(self, hw: Hw) -> Result<[bool; 256], i32> {
        let mut used = [false; 256];
        if let Some((start, span)) = self.pending {
            used[(start / PAGE) as usize..((start + span) / PAGE) as usize].fill(true);
        }
        self.areas(hw, |a| {
            for p in a.start / PAGE..a.end / PAGE {
                used[p as usize] = true;
            }
            Ok(())
        })?;
        Ok(used)
    }
    /// Every valid record, including dead ones; an uncommitted record still claims its length.
    pub fn records(
        self,
        hw: Hw,
        a: Area,
        mut visit: impl FnMut(u32, u8, u32) -> Result<(), i32>,
    ) -> Result<u32, i32> {
        let mut at = a.start + a.bytes;
        if a.start == 0 && a.kind == 0xe9 {
            at += 4; // The front runtime always reserves its boot marker word.
        }
        while at + 4 <= a.end {
            let mut h = [0; 4];
            self.read(hw, at, &mut h)?;
            if h[0] == 255 {
                break;
            }
            let n = u16::from_le_bytes([h[2], h[3]]) as u32;
            let kind = h[0] & 15;
            if h[0] & 128 == 0 || !(1..=5).contains(&kind) || 4 + align(n) > a.end - at {
                return Ok(a.end);
            }
            visit(at, h[0], n)?;
            at += 4 + align(n);
        }
        Ok(at)
    }
    pub fn files(self, hw: Hw, mut visit: impl FnMut(File) -> Result<(), i32>) -> Result<(), i32> {
        self.areas(hw, |a| {
            if a.kind == b'X' {
                return Ok(());
            }
            self.records(hw, a, |at, t, n| {
                if t & 0x40 != 0 || !(1..=3).contains(&(t & 15)) || n < 12 {
                    return Ok(());
                }
                let mut id = [0; 12];
                self.read(hw, at + 4, &mut id)?;
                visit(File {
                    name: id[..8].try_into().unwrap(),
                    version: u32le(&id[8..]),
                    size: n - 12,
                    at,
                    kind: t & 15,
                    cache: t & 32 == 0,
                    live: t & 16 != 0,
                })
            })?;
            Ok(())
        })
    }
    #[inline(never)]
    pub fn scan_lookup(self, hw: Hw, name: [u8; 8]) -> Result<Option<File>, i32> {
        let mut best: Option<File> = None;
        self.files(hw, |f| {
            if f.name == name
                && f.live
                && best.is_none_or(|b| {
                    f.version > b.version || (f.version == b.version && f.at > b.at)
                })
            {
                best = Some(f);
            }
            Ok(())
        })?;
        Ok(best)
    }
    pub fn scan_next(self, hw: Hw, after: Option<[u8; 8]>) -> Result<Option<File>, i32> {
        let mut name = None;
        self.files(hw, |f| {
            if f.live && after.is_none_or(|a| f.name > a) && name.is_none_or(|n| f.name < n) {
                name = Some(f.name);
            }
            Ok(())
        })?;
        match name {
            Some(n) => self.scan_lookup(hw, n),
            None => Ok(None),
        }
    }
    #[inline(never)]
    pub fn blank(self, hw: Hw, at: u32, len: u32) -> Result<bool, i32> {
        let mut b = [0; 1024];
        let mut p = 0;
        while p < len {
            let n = (len - p).min(1024) as usize;
            self.read(hw, at + p, &mut b[..n])?;
            if b[..n].iter().any(|v| *v != 255) {
                return Ok(false);
            }
            p += n as u32;
        }
        Ok(true)
    }
    pub fn free_page(self, hw: Hw, cursor: u32) -> Result<u32, i32> {
        let first = (cursor / PAGE).min(self.size / PAGE);
        let count = (self.size - FRONT) / PAGE;
        let used = self.occupied(hw)?;
        for step in 0..count {
            let p =
                FRONT + ((first.saturating_sub(FRONT / PAGE) + count - 1 - step) % count) * PAGE;
            // Dirty pages must be reclaimed deliberately; arbitrary bytes are never overwritten here.
            if !used[(p / PAGE) as usize] && self.blank(hw, p, PAGE)? {
                return Ok(p);
            }
        }
        Err(NOSPACE)
    }
    pub fn record_space(self, hw: Hw, len: u32) -> Result<(u32, bool), i32> {
        if len > 65516 {
            return Err(ERR_SIZE);
        }
        let mut found = None;
        self.areas(hw, |a| {
            if a.kind == b'D' && a.start >= FRONT {
                let end = self.records(hw, a, |_, _, _| Ok(()))?;
                let mut telemetry = false;
                self.records(hw, a, |_, t, _| {
                    telemetry |= t & 15 == 5;
                    Ok(())
                })?;
                if !telemetry
                    && a.end - end >= 4 + align(len)
                    && self.blank(hw, end, 4 + align(len))?
                {
                    found = Some(end);
                }
            }
            Ok(())
        })?;
        match found {
            Some(at) => Ok((at, false)),
            None => Ok((self.free_page(hw, self.cursor)? + 16, true)),
        }
    }
}

impl Layout {
    pub fn allocate(self, hw: Hw, pages: u32, exclude: u32) -> Result<u32, i32> {
        if pages == 0 || pages > self.size / PAGE {
            return Err(ERR_ARG);
        }
        let mut used = self.occupied(hw)?;
        if exclude < self.size {
            used[(exclude / PAGE) as usize] = true;
        }
        let count = (self.size - FRONT) / PAGE;
        for step in 0..count {
            let start = FRONT / PAGE
                + ((self.cursor / PAGE).saturating_sub(FRONT / PAGE) + count - 1 - step) % count;
            if start + pages <= self.size / PAGE
                && used[start as usize..(start + pages) as usize]
                    .iter()
                    .all(|v| !*v)
            {
                return Ok(start * PAGE);
            }
        }

        Err(NOSPACE)
    }
    pub fn verify_runtime(self, hw: Hw, at: u32, size: u32) -> Result<bool, i32> {
        if self.image_size(hw, at)? != Some(size) {
            return Ok(false);
        }
        let mut h = [0; 24];
        self.read(hw, at, &mut h)?;
        if h[0] != 0xe9 || u16::from_le_bytes([h[12], h[13]]) != crate::CPU as u16 {
            return Ok(false);
        }
        let mut pos = 24;
        let mut checksum = 0xef;
        let mut scratch = [0; 256];
        for _ in 0..h[1] {
            let mut seg = [0; 8];
            self.read(hw, at + pos, &mut seg)?;
            let len = u32le(&seg[4..]);
            pos += 8;
            let mut n = 0;
            while n < len {
                let chunk = (len - n).min(256) as usize;
                self.read(hw, at + pos + n, &mut scratch[..chunk])?;
                for b in &scratch[..chunk] {
                    checksum ^= *b;
                }
                n += chunk as u32;
            }
            pos += len;
        }
        let checksum_end = (pos | 15) + 1;
        let mut b = [0; 1];
        self.read(hw, at + checksum_end - 1, &mut b)?;
        if b[0] != checksum {
            return Ok(false);
        }
        if h[23] != 0 {
            let psa = crate::crypto::Psa::from_host(hw).ok_or(ERR_UNSUPPORTED)?;
            let mut hash = psa.sha256_stream()?;
            let mut pos = 0;
            while pos < checksum_end {
                let n = (checksum_end - pos).min(256) as usize;
                self.read(hw, at + pos, &mut scratch[..n])?;
                hash.update(&scratch[..n])?;
                pos += n as u32;
            }
            let expected = hash.finish()?;
            let mut actual = [0; 32];
            self.read(hw, at + checksum_end, &mut actual)?;
            if actual != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[derive(Clone, Copy)]
pub struct Batch {
    pub at: u32,
    pub size: u32,
    pub first: u64,
    pub count: u16,
}
impl Batch {
    pub fn last(self) -> u64 {
        self.first.saturating_add(self.count as u64 - 1)
    }
}
impl Layout {
    pub fn logs(self, hw: Hw, mut f: impl FnMut(Batch) -> Result<(), i32>) -> Result<(), i32> {
        self.areas(hw, |a| {
            if a.kind == b'X' {
                return Ok(());
            }
            self.records(hw, a, |at, t, n| {
                if t & 0x5f != 0x15 || n < 10 {
                    return Ok(());
                }
                let mut b = [0; 10];
                self.read(hw, at + 4, &mut b)?;
                let count = u16::from_le_bytes([b[8], b[9]]);
                if count == 0 {
                    return Ok(());
                }
                f(Batch {
                    at,
                    size: n - 10,
                    first: u64::from_le_bytes(b[..8].try_into().unwrap()),
                    count,
                })
            })?;
            Ok(())
        })
    }
    pub fn log_space(self, hw: Hw, len: u32) -> Result<(u32, bool), i32> {
        if len > 65516 {
            return Err(ERR_SIZE);
        }
        let mut found = None;
        self.areas(hw, |a| {
            if a.kind != b'D' {
                return Ok(());
            }
            let mut other = false;
            let end = self.records(hw, a, |_, t, _| {
                other |= t & 15 != 5;
                Ok(())
            })?;
            if !other && a.end - end >= 4 + align(len) && self.blank(hw, end, 4 + align(len))? {
                found = Some(end);
            }
            Ok(())
        })?;
        if let Some(at) = found {
            return Ok((at, false));
        }
        let used = self.occupied(hw)?;
        for page in (0..FRONT.min(self.size)).step_by(PAGE as usize).rev() {
            if !used[(page / PAGE) as usize] && self.blank(hw, page, PAGE)? {
                return Ok((page + 16, true));
            }
        }
        Ok((self.free_page(hw, self.cursor)? + 16, true))
    }
}

#[derive(Clone, Copy)]
struct Index {
    at: u32,
    run: u32,
    slots: u32,
}
impl Layout {
    #[inline(never)]
    fn index(self, hw: Hw) -> Result<Option<Index>, i32> {
        let mut best = None;
        let mut generation = 0;
        self.areas(hw, |a| {
            if a.kind != b'X' {
                return Ok(());
            }
            let mut h = [0; 16];
            self.read(hw, a.start, &mut h)?;
            let g = u32le(&h[8..]);
            self.records(hw, a, |at, t, n| {
                if t & 0x5f != 0x14 || n < 4 {
                    return Ok(());
                }
                let mut h = [0; 4];
                self.read(hw, at + 4, &mut h)?;
                let run = u16::from_le_bytes([h[0], h[1]]) as u32;
                let slots = u16::from_le_bytes([h[2], h[3]]) as u32;
                if 4 + 16 * (run + slots) != n {
                    return Err(ERR_IO);
                }
                if best.is_none() || g >= generation {
                    generation = g;
                    best = Some(Index {
                        at: at + 8,
                        run,
                        slots,
                    });
                }
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(best)
    }
    fn index_entry(self, hw: Hw, at: u32) -> Result<Option<[u8; 16]>, i32> {
        let mut e = [0; 16];
        self.read(hw, at, &mut e)?;
        if e[15] & 128 != 0 {
            return Ok(None);
        }
        if e[15] & 0x60 != 0x60 || !(1..=3).contains(&(e[15] & 15)) {
            return Err(ERR_IO);
        }
        Ok(Some(e))
    }
    #[inline(never)]
    fn indexed_file(self, hw: Hw, e: [u8; 16]) -> Result<Option<File>, i32> {
        let at = u32::from_le_bytes([e[12], e[13], e[14], 0])
            .checked_mul(4)
            .ok_or(ERR_IO)?;
        let mut h = [0; 16];
        self.read(hw, at, &mut h)?;
        if h[0] & 0x40 != 0 {
            return Ok(None);
        }
        let n = u16::from_le_bytes([h[2], h[3]]) as u32;
        if h[0] & 128 == 0
            || h[0] & 15 != e[15] & 15
            || n < 12
            || at % PAGE + 4 + align(n) > PAGE
            || h[4..12] != e[..8]
            || h[12..16] != e[8..12]
        {
            return Err(ERR_IO);
        }
        Ok(Some(File {
            name: e[..8].try_into().unwrap(),
            version: u32le(&e[8..]),
            size: n - 12,
            at,
            kind: h[0] & 15,
            cache: h[0] & 32 == 0,
            live: h[0] & 16 != 0,
        }))
    }
    fn lookup_index(self, hw: Hw, index: Index, name: [u8; 8]) -> Result<Option<File>, i32> {
        // Binary-search the immutable sorted run, then replay the bounded append log.
        let (mut lo, mut hi) = (0, index.run);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = self.index_entry(hw, index.at + 16 * mid)?.ok_or(ERR_IO)?;
            if e[..8] < name[..] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut best = None;
        let mut consider = |e: [u8; 16]| -> Result<(), i32> {
            if e[..8] == name[..] {
                if let Some(f) = self.indexed_file(hw, e)? {
                    // Deleted records are authoritative only as absence. They
                    // must not mask a later recreation whose version restarts.
                    if f.live && best.is_none_or(|b: File| f.version >= b.version) {
                        best = Some(f);
                    }
                }
            }
            Ok(())
        };
        while lo < index.run {
            let e = self.index_entry(hw, index.at + 16 * lo)?.ok_or(ERR_IO)?;
            if e[..8] != name[..] {
                break;
            }
            consider(e)?;
            lo += 1;
        }
        for i in 0..index.slots {
            let at = index.at + 16 * (index.run + i);
            let mut raw = [0; 16];
            self.read(hw, at, &mut raw)?;
            if raw == [255; 16] {
                break;
            }
            if let Some(e) = self.index_entry(hw, at)? {
                consider(e)?;
            }
        }
        Ok(best.filter(|f| f.live))
    }
    #[inline(never)]
    pub fn lookup(self, hw: Hw, name: [u8; 8]) -> Result<Option<File>, i32> {
        if let Ok(Some(index)) = self.index(hw) {
            if let Ok(found) = self.lookup_index(hw, index, name) {
                return Ok(found);
            }
        }
        self.scan_lookup(hw, name)
    }
    pub fn next(self, hw: Hw, after: Option<[u8; 8]>) -> Result<Option<File>, i32> {
        let Ok(Some(index)) = self.index(hw) else {
            return self.scan_next(hw, after);
        };
        let mut after = after;
        loop {
            let mut name = None;
            for i in 0..index.run + index.slots {
                let at = index.at + 16 * i;
                let mut raw = [0; 16];
                self.read(hw, at, &mut raw)?;
                if i >= index.run && raw == [255; 16] {
                    break;
                }
                let e = match self.index_entry(hw, at) {
                    Ok(Some(e)) => e,
                    Ok(None) => continue,
                    Err(_) => return self.scan_next(hw, after),
                };
                let key: [u8; 8] = e[..8].try_into().unwrap();
                if after.is_none_or(|a| key > a) && name.is_none_or(|n| key < n) {
                    name = Some(key);
                }
            }
            let Some(key) = name else {
                return Ok(None);
            };
            match self.lookup_index(hw, index, key) {
                Ok(Some(f)) => return Ok(Some(f)),
                Ok(None) => after = Some(key),
                Err(_) => return self.scan_next(hw, after),
            }
        }
    }
}
