//! The extent map of the `xip` partition, and its garbage collector.
//!
//! This is mod_flash's own implementation of the format that Stage2 and the loader read (`components/xip_loader/include/xip_extent.h`, C): the two
//! share nothing but the format, and `tests` below run the cases of `tests/host/test_extent.c`. Offsets are relative to the start of the partition.
//!
//! The first byte of every 64 KiB page says what starts there: `0xFF` empty, `0x00` deleted, `0xE9` a runtime (an ESP image), `M` a module, `D` a
//! data extent (`M` and `D` carry their length in bytes 4..8 and are skipped by it). A deleted extent has `0x00` programmed over the first byte of
//! each of its pages and is not erased: erasing is this module's job (`Map::plan`), done when the free space runs low or an install needs the pages.
use alloc::vec::Vec;

pub(crate) const PAGE: u32 = 65536;
pub(crate) const SECTOR: u32 = 4096;
/// What a runtime may take from its start (`XIP_RUNTIME_RESERVE`): the room its segment walk is believed within.
pub(crate) const RUNTIME_RESERVE: u32 = 0x180000;
pub(crate) const EMPTY: u8 = 0xff;
pub(crate) const DELETED: u8 = 0x00;
pub(crate) const RUNTIME: u8 = 0xe9;
pub(crate) const MODULE: u8 = b'M';
pub(crate) const DATA: u8 = b'D';
/// An erased page is needed to install; below this many the collector runs on its own (one module upgrade is usually one or two pages).
pub(crate) const LOW_WATER: usize = 4;

/// Reads of the partition (`Hw::flash_read` of the `xip` base in the module, a byte array in the tests).
pub(crate) trait Reader {
    fn read(&mut self, off: u32, buf: &mut [u8]) -> bool;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Extent {
    pub offset: u32,
    pub size: u32,
    pub kind: u8,
}
impl Extent {
    pub(crate) fn pages(&self) -> u32 {
        self.size / PAGE
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The size of the ESP image at `at`, rounded up to pages: the end of its last segment, the checksum byte's 16-byte block and the appended
/// SHA-256 (`xip_image_inspect`). `None` when it is not one or runs past `room`.
fn runtime_size<R: Reader>(r: &mut R, at: u32, room: u32) -> Option<u32> {
    let mut head = [0u8; 36];
    if room < head.len() as u32 || !r.read(at, &mut head) {
        return None;
    }
    let segments = head[1];
    if head[0] != RUNTIME || segments == 0 || segments > 16 || le32(&head[0x20..]) != 0xabcd_5432 {
        return None;
    }
    let mut pos = 24u32;
    for _ in 0..segments {
        let mut seg = [0u8; 8];
        if pos > room - 8 || !r.read(at + pos, &mut seg) {
            return None;
        }
        let len = le32(&seg[4..]);
        if len > room - pos - 8 {
            return None;
        }
        pos += 8 + len;
    }
    pos = (pos | 15) + 1;
    if head[23] != 0 {
        pos += 32;
    }
    (pos <= room).then_some(pos.div_ceil(PAGE) * PAGE)
}

/// What starts at `at` (a page start) in `room` bytes: its kind and size, or `None` for anything that is not an extent. The lengths are
/// believed only when they fit the room, are whole sectors for data and a multiple of 4 above a header's 64 bytes for a module.
pub(crate) fn probe<R: Reader>(r: &mut R, at: u32, room: u32) -> Option<Extent> {
    let mut h = [0u8; 8];
    if !r.read(at, &mut h) {
        return None;
    }
    let size = match h[0] {
        RUNTIME => runtime_size(r, at, room.min(RUNTIME_RESERVE))?,
        MODULE | DATA => {
            let len = le32(&h[4..]);
            let ok = if h[0] == MODULE { len > 64 && len % 4 == 0 } else { len != 0 && len % SECTOR == 0 };
            if !ok {
                return None;
            }
            len.div_ceil(PAGE).checked_mul(PAGE).filter(|s| *s <= room)?
        }
        _ => return None,
    };
    Some(Extent { offset: at, size, kind: h[0] })
}

/// How a page is used, from its first byte and the extents that cover it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Page {
    /// inside an extent
    Used,
    /// erased: first byte 0xFF
    Free,
    /// outside every extent and not erased: a deleted extent's page (0x00) or leftover of a cut write; an erase makes it free
    Dirty,
}

pub(crate) struct Map {
    pub extents: Vec<Extent>,
    pub pages: Vec<Page>,
}

/// One 8-byte read per page. An extent is skipped by its length; a page that is not the start of one advances by a page.
pub(crate) fn scan<R: Reader>(r: &mut R, size: u32) -> Option<Map> {
    let count = (size / PAGE) as usize;
    let mut map = Map { extents: Vec::new(), pages: Vec::new() };
    map.pages.try_reserve_exact(count).ok()?;
    map.pages.resize(count, Page::Free);
    let mut page = 0usize;
    while page < count {
        let at = page as u32 * PAGE;
        if let Some(e) = probe(r, at, size - at) {
            for p in page..page + e.pages() as usize {
                map.pages[p] = Page::Used;
            }
            page += e.pages() as usize;
            map.extents.try_reserve(1).ok()?;
            map.extents.push(e);
            continue;
        }
        let mut lead = [0u8; 1];
        if !r.read(at, &mut lead) {
            return None;
        }
        map.pages[page] = if lead[0] == EMPTY { Page::Free } else { Page::Dirty };
        page += 1;
    }
    Some(map)
}

impl Map {
    pub(crate) fn free(&self) -> usize {
        self.pages.iter().filter(|p| **p == Page::Free).count()
    }
    pub(crate) fn dirty(&self) -> usize {
        self.pages.iter().filter(|p| **p == Page::Dirty).count()
    }
    /// The space is low: fewer erased pages than `LOW_WATER` while there are pages to reclaim.
    pub(crate) fn low(&self) -> bool {
        self.free() < LOW_WATER && self.dirty() > 0
    }
    /// The first page to erase to get `needed` consecutive pages: `Some((start, dirty pages in the run))`, preferring a run that needs the fewest
    /// erases (and the lowest). The run's `Free` and `Dirty` pages only; `None` when no run of that length is available even after erasing.
    pub(crate) fn plan(&self, needed: usize) -> Option<(usize, Vec<usize>)> {
        if needed == 0 || needed > self.pages.len() {
            return None;
        }
        let mut best: Option<(usize, usize)> = None;
        for start in 0..=self.pages.len() - needed {
            let run = &self.pages[start..start + needed];
            if run.contains(&Page::Used) {
                continue;
            }
            let erases = run.iter().filter(|p| **p == Page::Dirty).count();
            if best.map_or(true, |b| erases < b.1) {
                best = Some((start, erases));
            }
            if erases == 0 {
                break;
            }
        }
        let (start, _) = best?;
        let dirty = (start..start + needed).filter(|p| self.pages[*p] == Page::Dirty).collect();
        Some((start, dirty))
    }
    /// The next dirty page to erase for the background collector (the lowest), `None` when there is none.
    pub(crate) fn next_dirty(&self) -> Option<usize> {
        self.pages.iter().position(|p| *p == Page::Dirty)
    }
}

/// The byte that deletes an extent's pages: written (a 1 to 0 program, no erase) at `Extent::offset + n * PAGE` for every page, the header
/// page first so that a power cut never leaves a valid-looking but partly deleted extent.
pub(crate) fn delete_offsets(e: &Extent) -> impl Iterator<Item = u32> + '_ {
    (0..e.pages()).map(move |n| e.offset + n * PAGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    const PAGES: usize = 24;
    struct Fake(Vec<u8>);
    impl Reader for Fake {
        fn read(&mut self, off: u32, buf: &mut [u8]) -> bool {
            let o = off as usize;
            if o + buf.len() > self.0.len() {
                return false;
            }
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            true
        }
    }
    fn blank() -> Fake {
        Fake(vec![0xff; PAGES * PAGE as usize])
    }
    fn extent_at(f: &mut Fake, page: usize, kind: u8, len: u32) {
        let p = page * PAGE as usize;
        f.0[p] = kind;
        f.0[p + 1] = 4;
        f.0[p + 2] = 64;
        f.0[p + 3] = 0;
        f.0[p + 4..p + 8].copy_from_slice(&len.to_le_bytes());
    }
    fn runtime_at(f: &mut Fake, page: usize, body: u32) {
        let p = page * PAGE as usize;
        f.0[p] = RUNTIME;
        f.0[p + 1] = 1;
        f.0[p + 23] = 1;
        f.0[p + 28..p + 32].copy_from_slice(&body.to_le_bytes());
        f.0[p + 32..p + 36].copy_from_slice(&0xabcd_5432u32.to_le_bytes());
    }
    fn delete(f: &mut Fake, page: usize, pages: usize) {
        for i in 0..pages {
            f.0[(page + i) * PAGE as usize] = DELETED;
        }
    }
    fn kinds(m: &Map) -> Vec<(u8, u32, u32)> {
        m.extents.iter().map(|e| (e.kind, e.offset / PAGE, e.pages())).collect()
    }
    fn size() -> u32 {
        (PAGES as u32) * PAGE
    }

    #[test]
    fn the_cases_of_the_c_scan() {
        let mut f = blank();
        assert!(scan(&mut f, size()).unwrap().extents.is_empty());

        runtime_at(&mut f, 0, 0x40000); // 4 pages and the header: 5
        extent_at(&mut f, 6, MODULE, 0x10004);
        extent_at(&mut f, 9, MODULE, 0x2000);
        extent_at(&mut f, 11, DATA, 0x20000);
        runtime_at(&mut f, PAGES - 3, 0x20000);
        f.0[3 * PAGE as usize..3 * PAGE as usize + 64].fill(RUNTIME); // a body page that looks like an image start
        let m = scan(&mut f, size()).unwrap();
        assert_eq!(kinds(&m), vec![(RUNTIME, 0, 5), (MODULE, 6, 2), (MODULE, 9, 1), (DATA, 11, 2), (RUNTIME, PAGES as u32 - 3, 3)]);

        // deleted: every page of the extent starts with 0x00, none is an extent, and they are dirty until erased
        delete(&mut f, 0, 5);
        delete(&mut f, 6, 2);
        let m = scan(&mut f, size()).unwrap();
        assert_eq!(kinds(&m), vec![(MODULE, 9, 1), (DATA, 11, 2), (RUNTIME, PAGES as u32 - 3, 3)]);
        assert_eq!(m.dirty(), 7);
        for p in 0..5 {
            f.0[p * PAGE as usize..(p + 1) * PAGE as usize].fill(0xff);
        }
        assert_eq!(scan(&mut f, size()).unwrap().dirty(), 2);
    }

    #[test]
    fn lengths_that_are_not_believed() {
        for (kind, len) in [(MODULE, 0x4000000u32), (MODULE, 10), (MODULE, 0x2002), (DATA, 0x400004), (DATA, 0x2100)] {
            let mut f = blank();
            extent_at(&mut f, 14, kind, len);
            let m = scan(&mut f, size()).unwrap();
            assert!(m.extents.is_empty(), "{kind} {len:#x}");
            assert_eq!(m.dirty(), 1); // not an extent, not erased: reclaimable
        }
        let mut f = blank();
        runtime_at(&mut f, 20, 0x500000); // segments run off the room
        assert!(scan(&mut f, size()).unwrap().extents.is_empty());
    }

    #[test]
    fn a_runtime_hides_no_more_than_the_reserve() {
        let mut f = blank();
        runtime_at(&mut f, 0, 0x3fffff0);
        assert!(scan(&mut f, size()).unwrap().extents.is_empty());
    }

    #[test]
    fn space_is_low_when_few_pages_are_erased_and_some_can_be() {
        let mut f = blank();
        for p in 0..PAGES - 2 {
            extent_at(&mut f, p, MODULE, 0x1000);
        }
        let m = scan(&mut f, size()).unwrap();
        assert_eq!((m.free(), m.dirty(), m.low()), (2, 0, false)); // low but nothing to reclaim: the collector has nothing to do
        delete(&mut f, 3, 1);
        delete(&mut f, 5, 1);
        let m = scan(&mut f, size()).unwrap();
        assert_eq!((m.free(), m.dirty(), m.low()), (2, 2, true));
        assert_eq!(m.next_dirty(), Some(3));
    }

    #[test]
    fn plan_prefers_the_run_that_needs_the_fewest_erases() {
        let mut f = blank();
        for p in 0..PAGES {
            extent_at(&mut f, p, MODULE, 0x1000);
        }
        let used = |f: &mut Fake, p: usize| extent_at(f, p, MODULE, 0x1000);
        delete(&mut f, 2, 2); // pages 2,3 dirty
        delete(&mut f, 10, 1);
        f.0[11 * PAGE as usize..12 * PAGE as usize].fill(0xff); // 11 free
        f.0[12 * PAGE as usize..13 * PAGE as usize].fill(0xff); // 12 free
        let m = scan(&mut f, size()).unwrap();
        assert_eq!(m.plan(2), Some((11, vec![])), "two erased pages in a row need no erase");
        assert_eq!(m.plan(3), Some((10, vec![10])));
        assert_eq!(m.plan(4), None, "no four consecutive pages even after erasing");
        assert_eq!(m.plan(0), None);
        used(&mut f, 12);
        let m = scan(&mut f, size()).unwrap();
        assert_eq!(m.plan(2), Some((10, vec![10])), "pages 2 and 3 are both dirty (two erases); 10 and 11 need one");
    }

    #[test]
    fn delete_marks_every_page_header_first() {
        let e = Extent { offset: 3 * PAGE, size: 3 * PAGE, kind: RUNTIME };
        assert_eq!(delete_offsets(&e).collect::<Vec<_>>(), vec![3 * PAGE, 4 * PAGE, 5 * PAGE]);
    }
}
