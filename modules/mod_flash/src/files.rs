//! Stream and direct packet filesystem frontends sharing one serialized writer.
use crate::store::{self, File, Layout, MAX_FILE, NOSPACE, PAGE, VERSION};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use xip_abi::{
    cbor, CallView, Hw, ERR_ARG, ERR_BUSY, ERR_IO, ERR_NOMEM, ERR_NOTFOUND, ERR_SIZE,
    ERR_UNSUPPORTED, PKT_MAX, STREAM_CLOSED, STREAM_DATA, STREAM_FIN, STREAM_OPEN,
};
const EVENT: i32 = 103;
pub const LOCAL_MAX: u32 = xip_abi::FILES_LOCAL_MAX;
pub const FILES_COMPONENT: i32 = xip_abi::FILES_COMPONENT as i32;
#[derive(Clone, Copy)]
struct Request {
    pkt: u32,
    len: usize,
    from: u32,
    component: i32,
    method: i32,
    id: u64,
}
#[derive(Clone, Copy)]
struct Local {
    request: Request,
    body_at: usize,
    body_len: usize,
}
fn bytes(data: &[u8], tag: u64) -> Result<&[u8], i32> {
    match cbor::get(data, tag)
        .map_err(|_| ERR_ARG)?
        .and_then(|mut r| r.next())
    {
        Some(cbor::Item::Bytes(b)) => Ok(b),
        _ => Err(ERR_ARG),
    }
}
fn local_method(method: i32) -> Option<i32> {
    Some(match method {
        17 => 3,
        18 => 4,
        19 => 10,
        20 => 11,
        2 | 5 | 6 | 7 | 8 | 12 => method,
        _ => return None,
    })
}

#[derive(Clone)]
enum Work {
    Program(u32, Vec<u8>),
    Erase(u32, u32),
    Copy(u32, u32, u32),
    CommitModule(u32, [u8; 4]),
    Rescan,
}
struct Build {
    page: u32,
    count: u16,
    index: u16,
    after: Option<[u8; 8]>,
    old: Vec<u32>,
}
struct Collect {
    source: store::Area,
    dest: u32,
    after: u32,
}
struct Collection {
    bytes: u32,
    full: bool,
    erase: bool,
    done: bool,
    seen: [u8; 32],
    pinned: [u8; 32],
    dest: u32,
    processed: u32,
    erased: u32,
}
impl Collection {
    fn has(bits: &[u8; 32], page: u32) -> bool {
        bits[page as usize / 8] & (1 << (page % 8)) != 0
    }
    fn mark(bits: &mut [u8; 32], page: u32) {
        bits[page as usize / 8] |= 1 << (page % 8);
    }
}
struct Transfer {
    local: Option<Local>,
    handle: u32,
    tcp: bool,
    sent: u32,
    done: bool,
    method: i32,
    layout: Layout,
    queue: VecDeque<Work>,
    waiting: bool,
    job: u64,
    error: i32,
    at: u32,
    record: u32,
    size: u32,
    received: u32,
    carry: [u8; 4],
    carried: usize,
    fin: bool,
    committing: bool,
    name: [u8; 8],
    version: u32,
    kind: u8,
    cache: bool,
    old: Option<File>,
    index: u32,
    build: Option<Build>,
    collect: Option<Collect>,
    collection: Option<Collection>,
    removing: bool,
    sweep: Option<[u8; 256]>,
    erasing_logs: Option<u64>,
    list_after: Option<[u8; 8]>,
    first_seq: u64,
    count: u16,
    since: u64,
    batch: Option<store::Batch>,
    upload: Option<Upload>,
    image_checked: bool,
    skip_body: bool,
    reply: bool,
    out: u32,
    out_len: u32,
    out_fin: u32,
    read_at: u32,
    read_len: u32,
}
#[derive(Default)]
pub struct Files {
    retry: Option<Upload>,
    queued: VecDeque<Request>,
    transfer: Option<Transfer>,
    cursor: Option<u32>,
}
// Retry metadata is volatile; reset recovery relies only on native image state.
#[derive(Clone, Copy)]
struct Upload {
    id: u64,
    method: i32,
    start: u32,
    size: u32,
    version: u32,
    name: [u8; 8],
}
impl Upload {
    fn span(self) -> u32 {
        if self.start == 0 && self.method == 14 {
            store::FRONT
        } else {
            self.size.div_ceil(PAGE) * PAGE
        }
    }
}
fn integer(fields: &[u8], key: u64, default: u32) -> Result<u32, i32> {
    match cbor::get_int(fields, key).map_err(|_| ERR_ARG)? {
        None => Ok(default),
        Some(v) => u32::try_from(v).map_err(|_| ERR_ARG),
    }
}
fn name(fields: &[u8]) -> Result<[u8; 8], i32> {
    let Some(mut r) = cbor::get(fields, 1).map_err(|_| ERR_ARG)? else {
        return Err(ERR_ARG);
    };
    let Some(cbor::Item::Bytes(b)) = r.next() else {
        return Err(ERR_ARG);
    };
    if b.is_empty() || b.len() > 8 {
        return Err(ERR_ARG);
    }
    let mut out = [0; 8];
    out[..b.len()].copy_from_slice(b);
    Ok(out)
}
fn flag(fields: &[u8], key: u64) -> Result<bool, i32> {
    match cbor::get(fields, key).map_err(|_| ERR_ARG)? {
        None => Ok(false),
        Some(mut r) => match r.next() {
            Some(cbor::Item::Bool(b)) => Ok(b),
            _ => Err(ERR_ARG),
        },
    }
}
fn word(t: u8, len: u32) -> Vec<u8> {
    alloc::vec![t, 255, len as u8, (len >> 8) as u8]
}
fn entry(f: File) -> [u8; 16] {
    let mut e = [255; 16];
    e[..8].copy_from_slice(&f.name);
    e[8..12].copy_from_slice(&f.version.to_le_bytes());
    let loc = f.at / 4;
    e[12..15].copy_from_slice(&loc.to_le_bytes()[..3]);
    e[15] = 0x60 | f.kind | if f.cache { 0 } else { 16 };
    e
}
impl Transfer {
    fn program(&mut self, at: u32, b: &[u8]) {
        self.queue.push_back(Work::Program(at, b.to_vec()));
    }
    fn index_slot(&mut self, hw: Hw, exclude: u32) -> Result<(), i32> {
        let l = self.layout;
        let mut best = None;
        let mut generation = 0;
        let mut old = Vec::new();
        let mut damaged = false;
        l.areas(hw, |a| {
            if a.kind != b'X' {
                return Ok(());
            }
            old.push(a.start);
            let mut h = [0; 16];
            l.read(hw, a.start, &mut h)?;
            let gen = store::u32le(&h[8..]);
            l.records(hw, a, |at, t, n| {
                if t & 15 != 4 || t & 0x40 != 0 || n < 4 {
                    return Ok(());
                }
                let mut count = [0; 4];
                l.read(hw, at + 4, &mut count)?;
                let run = u16::from_le_bytes([count[0], count[1]]) as u32;
                let slots = u16::from_le_bytes([count[2], count[3]]) as u32;
                if 4 + 16 * (run + slots) != n {
                    damaged = true;
                    return Ok(());
                }
                if gen >= generation {
                    generation = gen;
                    best = None;
                    for i in 0..slots {
                        let slot = at + 8 + 16 * (run + i);
                        if l.blank(hw, slot, 16)? {
                            best = Some(slot);
                            break;
                        }
                    }
                }
                Ok(())
            })?;
            Ok(())
        })?;
        if let Some(at) = best.filter(|_| !damaged) {
            self.index = at;
            return Ok(());
        }
        let page = l.free_page(hw, exclude)?;
        if page == exclude {
            return Err(NOSPACE);
        }
        let mut count = 0u16;
        let mut after = None;
        while let Some(f) = l.scan_next(hw, after)? {
            count = count.checked_add(1).ok_or(NOSPACE)?;
            after = Some(f.name);
        }
        if count as u32 + 256 > 4094 {
            return Err(NOSPACE);
        }
        let mut header = [255; 16];
        header[0] = b'X';
        header[4..8].copy_from_slice(&PAGE.to_le_bytes());
        header[8..12].copy_from_slice(&generation.checked_add(1).ok_or(ERR_SIZE)?.to_le_bytes());
        self.program(page, &header);
        let n = 4 + 16 * (count as u32 + 256);
        let mut rec = word(0xf4, n);
        rec.extend_from_slice(&count.to_le_bytes());
        rec.extend_from_slice(&256u16.to_le_bytes());
        self.program(page + 16, &rec);
        self.index = page + 24 + 16 * count as u32;
        self.build = Some(Build {
            page,
            count,
            index: 0,
            after: None,
            old,
        });
        Ok(())
    }
    #[inline(never)]
    fn open(
        hw: Hw,
        c: &CallView,
        cursor: Option<u32>,
        tcp: bool,
        retry: Option<Upload>,
    ) -> Result<Self, i32> {
        let mut l = Layout::discover(hw)?;
        if !matches!(c.method, 13 | 14) {
            l.pending = retry.map(|r| (r.start, r.span()));
        }
        if let Some(cursor) = cursor {
            l.cursor = cursor;
        } else {
            let mut newest = None;
            l.logs(hw, |b| {
                if newest.is_none_or(|n: store::Batch| b.last() > n.last()) {
                    newest = Some(b);
                }
                Ok(())
            })?;
            if let Some(b) = newest {
                if b.at >= store::FRONT {
                    l.cursor = b.at / PAGE * PAGE;
                }
            }
        }
        let mut t = Self {
            local: None,
            handle: c.id as u32,
            tcp,
            sent: 0,
            done: false,
            method: c.method,
            layout: l,
            queue: VecDeque::new(),
            waiting: false,
            job: 0,
            error: 0,
            at: 0,
            record: 0,
            size: 0,
            received: 0,
            carry: [255; 4],
            carried: 0,
            fin: false,
            committing: false,
            name: [0; 8],
            version: 0,
            kind: 1,
            cache: false,
            old: None,
            index: 0,
            build: None,
            collect: None,
            collection: None,
            removing: false,
            sweep: None,
            erasing_logs: None,
            list_after: None,
            first_seq: 0,
            count: 0,
            since: 0,
            batch: None,
            upload: None,
            image_checked: false,
            skip_body: false,
            reply: false,
            out: 0,
            out_len: 0,
            out_fin: 0,
            read_at: 0,
            read_len: 0,
        };
        match c.method {
            2 | 3 | 4 | 5 => {
                t.name = name(c.data)?;
                t.old = l.lookup(hw, t.name)?;
                if let Some(mut r) = cbor::get(c.data, 5).map_err(|_| ERR_ARG)? {
                    let Some(cbor::Item::Uint(expect)) = r.next() else {
                        return Err(ERR_ARG);
                    };
                    if expect != t.old.map_or(0, |f| f.version as u64) {
                        return Err(VERSION);
                    }
                }
                if c.method != 4 && t.old.is_none() {
                    return Err(ERR_NOTFOUND);
                }
                if let Some(f) = t.old {
                    t.version = f.version;
                    t.size = f.size;
                    t.kind = f.kind;
                    t.cache = f.cache;
                }
                if c.method == 3 {
                    let f = t.old.unwrap();
                    let offset = integer(c.data, 6, 0)?.min(f.size);
                    t.read_at = f.at + 16 + offset;
                    t.read_len = integer(c.data, 7, f.size - offset)?.min(f.size - offset);
                }
                if c.method == 4 {
                    t.size = integer(c.data, 2, u32::MAX)?;
                    if t.size > MAX_FILE {
                        return Err(ERR_SIZE);
                    }
                    t.kind = integer(c.data, 3, 1)? as u8;
                    if !(1..=3).contains(&t.kind) {
                        return Err(ERR_ARG);
                    }
                    t.cache = flag(c.data, 4)?;
                    t.version = t
                        .old
                        .map_or(Some(1), |f| f.version.checked_add(1))
                        .ok_or(ERR_SIZE)?;
                    let (record, fresh) = l.record_space(hw, t.size + 12)?;
                    t.record = record;
                    t.at = record + 16;
                    t.index_slot(hw, record / PAGE * PAGE)?;
                    if fresh {
                        let page = record - 16;
                        let mut h = [255; 16];
                        h[0] = b'D';
                        h[4..8].copy_from_slice(&PAGE.to_le_bytes());
                        t.program(page, &h);
                    }
                    let mut h = word(
                        0xc0 | 16 | if t.cache { 0 } else { 32 } | t.kind,
                        t.size + 12,
                    );
                    h.extend_from_slice(&t.name);
                    h.extend_from_slice(&t.version.to_le_bytes());
                    t.program(record, &h);
                }
            }
            6 => {
                t.size = integer(c.data, 2, 16)?.min(32);
                if cbor::get(c.data, 1).map_err(|_| ERR_ARG)?.is_some() {
                    t.list_after = Some(name(c.data)?);
                }
            }
            7 | 8 => {}
            23 => { t.at = integer(c.data, 6, 0)?; if t.at > l.size || t.at % PAGE != 0 { return Err(ERR_ARG); } }
            9 => {
                let bytes = integer(c.data, 2, 0)?;
                let full = flag(c.data, 3)?;
                let erase = cbor::get(c.data, 4).map_err(|_| ERR_ARG)?.is_none()
                    || flag(c.data, 4)?;
                if bytes > l.size - store::FRONT || (full && bytes != 0) {
                    return Err(ERR_ARG);
                }
                let mut plan = Collection {
                    bytes, full, erase, done: false, seen: [0; 32], pinned: [0; 32],
                    dest: 0, processed: 0, erased: 0,
                };
                let (running, span) = crate::running_partition(hw).ok_or(ERR_UNSUPPORTED)?;
                let start = running.max(l.base).min(l.base + l.size) - l.base;
                let end = running.saturating_add(span).min(l.base + l.size).saturating_sub(l.base);
                for page in start / PAGE..end.div_ceil(PAGE) {
                    Collection::mark(&mut plan.pinned, page);
                }
                let mut failed = false;
                if !crate::for_modules(hw, |_, _, live, page| {
                    if !live { return; }
                    if let Some(page) = page {
                        let abs = page.saturating_mul(PAGE as u64);
                        if abs >= l.base as u64 && abs < (l.base + l.size) as u64 {
                            let at = abs as u32 - l.base;
                            let mut h = [0; 8];
                            if l.read(hw, at, &mut h).is_err() { failed = true; return; }
                            let span = store::u32le(&h[4..]);
                            let pages = if span > 64 && span <= l.size - at {
                                span.div_ceil(PAGE)
                            } else { 1 };
                            for p in at / PAGE..at / PAGE + pages {
                                Collection::mark(&mut plan.pinned, p);
                            }
                        }
                    }
                }) || failed { return Err(ERR_IO); }
                t.collection = Some(plan);
            }
            10 => {
                t.size = integer(c.data, 2, u32::MAX)?;
                if t.size > 65506 {
                    return Err(ERR_SIZE);
                }
                t.first_seq = match cbor::get(c.data, 10)
                    .map_err(|_| ERR_ARG)?
                    .and_then(|mut r| r.next())
                {
                    Some(cbor::Item::Uint(v)) => v,
                    _ => return Err(ERR_ARG),
                };
                t.count = u16::try_from(integer(c.data, 11, 0)?).map_err(|_| ERR_ARG)?;
                if t.count == 0 || t.first_seq.checked_add(t.count as u64 - 1).is_none() {
                    return Err(ERR_ARG);
                }
                let (at, fresh) = l.log_space(hw, t.size + 10)?;
                t.record = at;
                t.at = at + 12;
                if fresh {
                    let mut h = [255; 16];
                    h[0] = b'D';
                    h[4..8].copy_from_slice(&PAGE.to_le_bytes());
                    t.program(at - 16, &h);
                }
                let mut h = word(0xf5, t.size + 10);
                h.extend_from_slice(&t.first_seq.to_le_bytes());
                t.program(at, &h);
                t.carry[..2].copy_from_slice(&t.count.to_le_bytes());
                t.carried = 2;
            }
            11 => {
                t.since = match cbor::get(c.data, 10)
                    .map_err(|_| ERR_ARG)?
                    .and_then(|mut r| r.next())
                {
                    None => 0,
                    Some(cbor::Item::Uint(v)) => v,
                    _ => return Err(ERR_ARG),
                };
                t.next_batch(hw)?;
            }
            12 => {
                let through = match cbor::get(c.data, 10)
                    .map_err(|_| ERR_ARG)?
                    .and_then(|mut r| r.next())
                {
                    Some(cbor::Item::Uint(v)) => v,
                    _ => return Err(ERR_ARG),
                };
                t.erasing_logs = Some(through);
            }
            15 => {
                let start = integer(c.data, 6, u32::MAX)?;
                if start % PAGE != 0 {
                    return Err(ERR_ARG);
                }
                let mut source = None;
                l.areas(hw, |a| {
                    if a.start == start && matches!(a.kind, b'M' | 0xe9) {
                        source = Some(a);
                    }
                    Ok(())
                })?;
                let source = source.ok_or(ERR_NOTFOUND)?;
                t.protect(hw, source.start, source.end - source.start)?;
                let mut running = false;
                if !crate::for_modules(hw, |_, _, live, page| {
                    if live {
                        if let Some(page) = page {
                            let at = (page as u32).saturating_mul(PAGE);
                            running |= at >= l.base + source.start && at < l.base + source.end;
                        }
                    }
                }) {
                    return Err(ERR_IO);
                }
                if running {
                    return Err(ERR_BUSY);
                }
                let mut retained = false;
                l.records(hw, source, |_, ty, _| {
                    retained |= ty & 0x70 == 0x30 && (1..=3).contains(&(ty & 15));
                    Ok(())
                })?;
                if retained {
                    let dest = l.free_page(hw, l.cursor)?;
                    t.index_slot(hw, dest)?;
                    let mut h = [255; 16];
                    h[0] = b'D';
                    h[4..8].copy_from_slice(&PAGE.to_le_bytes());
                    t.program(dest, &h);
                    t.collect = Some(Collect {
                        source,
                        dest: dest + 16,
                        after: source.start + source.bytes,
                    });
                } else {
                    t.retire(hw, source)?;
                }
            }
            13 | 14 => {
                t.name = if c.method == 13 {
                    name(c.data)?
                } else {
                    *b"runtime\0"
                };
                t.size = integer(c.data, 2, 0)?;
                t.version = integer(c.data, 9, 0)?;
                if t.size == 0 || t.size.div_ceil(PAGE) > 255 || t.version == 0 {
                    return Err(ERR_ARG);
                }
                let id = match cbor::get(c.data, 8)
                    .map_err(|_| ERR_ARG)?
                    .and_then(|mut r| r.next())
                {
                    Some(cbor::Item::Uint(id)) => id,
                    _ => return Err(ERR_ARG),
                };
                let front = flag(c.data, 12)?;
                if front && (c.method != 14 || t.size + 4 > store::FRONT) {
                    return Err(ERR_ARG);
                }
                let previous = retry.filter(|r| r.id == id);
                if let Some(r) = previous {
                    if r.name != t.name
                        || r.size != t.size
                        || r.method != c.method
                        || r.version != t.version
                        || (r.start == 0) != front
                    {
                        return Err(ERR_ARG);
                    }
                    t.record = r.start;
                    t.skip_body = t.image_valid(hw)?;
                }
                // Image identity survives reset; operation IDs do not need to.
                if !t.skip_body {
                    l.areas(hw, |a| {
                        if !t.skip_body
                            && a.kind == if c.method == 13 { b'M' } else { 0xe9 }
                            && a.bytes == store::align(t.size)
                            && (c.method != 14 || (a.start == 0) == front)
                        {
                            t.record = a.start;
                            t.skip_body = t.image_valid(hw)?;
                        }
                        Ok(())
                    })?;
                }
                if !t.skip_body {
                    if let Some(r) = previous {
                        t.record = r.start;
                    } else {
                        if c.method == 14 {
                            let mut images = 0;
                            l.areas(hw, |a| {
                                images += (a.kind == 0xe9) as u32;
                                Ok(())
                            })?;
                            if images >= 2 {
                                return Err(ERR_BUSY);
                            }
                        }
                        t.record = if front {
                            0
                        } else {
                            l.allocate(hw, t.size.div_ceil(PAGE), u32::MAX)?
                        };
                    }
                }
                let upload = Upload {
                    id,
                    method: c.method,
                    start: t.record,
                    size: t.size,
                    version: t.version,
                    name: t.name,
                };
                t.at = t.record;
                t.upload = Some(upload);
                if !t.skip_body {
                    t.protect(hw, upload.start, upload.span())?;
                    t.queue.push_back(Work::Erase(upload.start, upload.span()));
                }
                if !t.skip_body && c.method == 13 {
                    let mut claim = [255; 8];
                    claim[0] = 0x4f;
                    claim[4..].copy_from_slice(&t.size.to_le_bytes());
                    t.program(t.record, &claim);
                }
            }
            _ => return Err(ERR_UNSUPPORTED),
        }
        Ok(t)
    }
    fn open_local(
        hw: Hw,
        request: Request,
        cursor: Option<u32>,
        retry: Option<Upload>,
    ) -> Result<Self, i32> {
        let (ptr, _) = hw.pkt_buf(request.pkt)?;
        let data = unsafe { core::slice::from_raw_parts(ptr, request.len) };
        let method = local_method(request.method).ok_or(ERR_UNSUPPORTED)?;
        let content = if matches!(request.method, 18 | 19) {
            bytes(data, 13)?
        } else {
            &[]
        };
        if content.len() > LOCAL_MAX as usize {
            return Err(ERR_ARG);
        }
        let mut fields = [0; 256];
        let mut reader = cbor::Reader::new(data);
        let Some(cbor::Item::Map(count)) = reader.next() else {
            return Err(ERR_ARG);
        };
        let mut kept = 0;
        let replaced = |key| {
            key == 13
                || (matches!(request.method, 18 | 19) && key == 2)
                || (request.method == 17 && key == 7)
        };
        for _ in 0..count {
            let Some(cbor::Item::Uint(key)) = reader.next() else {
                return Err(ERR_ARG);
            };
            if !replaced(key) {
                kept += 1;
            }
            if !reader.skip() {
                return Err(ERR_ARG);
            };
        }
        let extra = matches!(request.method, 17 | 18 | 19) as usize;
        let mut w = cbor::Writer::new(&mut fields);
        w.map(kept + extra);
        reader = cbor::Reader::new(data);
        reader.next();
        for _ in 0..count {
            let Some(cbor::Item::Uint(key)) = reader.next() else {
                return Err(ERR_ARG);
            };
            let start = reader.rest();
            reader.skip();
            if !replaced(key) {
                w.uint(key).raw(&start[..start.len() - reader.rest().len()]);
            }
        }
        if matches!(request.method, 18 | 19) {
            w.uint(2).uint(content.len() as u64);
        }
        if request.method == 17 {
            let len = integer(data, 7, LOCAL_MAX)?;
            if len > LOCAL_MAX {
                return Err(ERR_ARG);
            };
            w.uint(7).uint(len as u64);
        }
        let call = CallView {
            component: 44,
            method,
            value: STREAM_OPEN,
            flags: xip_abi::CALL_STREAM,
            data: w.finish().ok_or(ERR_SIZE)?,
            time_us: 0,
            pkt: 0,
            from: request.from,
            id: request.id,
            request_id: None,
            src: None,
        };
        let mut t = Self::open(hw, &call, cursor, false, retry)?;
        if request.method == 20 {
            t.size = integer(data, 13, LOCAL_MAX)?;
            if t.size > LOCAL_MAX {
                return Err(ERR_ARG);
            };
        }
        t.local = Some(Local {
            request,
            body_at: if content.is_empty() {
                0
            } else {
                content.as_ptr() as usize - ptr as usize
            },
            body_len: content.len(),
        });
        t.fin = !matches!(request.method, 18 | 19);
        Ok(t)
    }
    fn local_logs(&self, hw: Hw, w: &mut cbor::Writer) -> Result<(), i32> {
        let (mut oldest, mut newest) = (u64::MAX, 0);
        self.layout.logs(hw, |b| {
            oldest = oldest.min(b.first);
            newest = newest.max(b.last());
            Ok(())
        })?;
        let mut batches = Vec::new();
        let (mut since, mut total, mut wire) = (self.since, 0, 64);
        loop {
            let mut next = None;
            self.layout.logs(hw, |b| {
                if b.last() > since && next.is_none_or(|n: store::Batch| b.first < n.first) {
                    next = Some(b)
                };
                Ok(())
            })?;
            let Some(b) = next else { break };
            if total + b.size > self.size || wire + b.size + 32 > PKT_MAX {
                break;
            };
            total += b.size;
            wire += b.size + 32;
            since = b.last();
            batches.push(b);
        }
        w.map(3)
            .uint(1)
            .uint(newest)
            .uint(2)
            .uint(if oldest == u64::MAX { 0 } else { oldest })
            .uint(14)
            .array(batches.len());
        let mut data = [0; LOCAL_MAX as usize];
        for b in batches {
            if b.size != 0 {
                self.layout
                    .read(hw, b.at + 14, &mut data[..b.size as usize])?;
            }
            w.map(3)
                .uint(1)
                .uint(b.first)
                .uint(2)
                .uint(b.count as u64)
                .uint(13)
                .bytes(&data[..b.size as usize]);
        }
        Ok(())
    }
    fn next_batch(&mut self, hw: Hw) -> Result<(), i32> {
        let mut best = None;
        self.layout.logs(hw, |b| {
            if b.last() > self.since && best.is_none_or(|v: store::Batch| b.first < v.first) {
                best = Some(b);
            }
            Ok(())
        })?;
        self.batch = best;
        if let Some(b) = best {
            self.read_at = b.at + 14;
            self.read_len = b.size;
            self.first_seq = b.first;
            self.count = b.count;
        }
        Ok(())
    }
    fn collection_used(&self, hw: Hw) -> Result<[bool; 256], i32> {
        let mut used = self.layout.occupied(hw)?;
        let plan = self.collection.as_ref().ok_or(ERR_ARG)?;
        for p in 0..self.layout.size / PAGE {
            used[p as usize] |= Collection::has(&plan.pinned, p);
        }
        Ok(used)
    }
    fn collection_space(&self, hw: Hw) -> Result<(u32, u32, u32), i32> {
        let used = self.collection_used(hw)?;
        let erase = self.collection.as_ref().ok_or(ERR_ARG)?.erase;
        let (mut free, mut dirty, mut run, mut largest) = (0, 0, 0, 0);
        for p in store::FRONT / PAGE..self.layout.size / PAGE {
            if used[p as usize] { run = 0; continue; }
            let blank = self.layout.blank(hw, p * PAGE, PAGE)?;
            if blank { free += 1; } else { dirty += 1; }
            if blank || !erase { run += PAGE; largest = largest.max(run); }
            else { run = 0; }
        }
        Ok((free, dirty, largest))
    }
    fn collection_free_page(&self, hw: Hw) -> Result<u32, i32> {
        let l = self.layout;
        let used = self.collection_used(hw)?;
        let count = (l.size - store::FRONT) / PAGE;
        for step in 0..count {
            let page = store::FRONT / PAGE
                + ((l.cursor / PAGE).saturating_sub(store::FRONT / PAGE) + count - 1 - step) % count;
            if !used[page as usize] && l.blank(hw, page * PAGE, PAGE)? {
                return Ok(page * PAGE);
            }
        }
        Err(NOSPACE)
    }
    fn reclaim(&mut self, hw: Hw, at: u32, size: u32) -> Result<(), i32> {
        if self.collection.as_ref().is_some_and(|c| !c.erase) {
            let mut h = [0; 4];
            self.layout.read(hw, at, &mut h)?;
            h[0] = 0; // Length remains intact, so the scan skips the retired extent.
            self.program(at, &h);
        } else {
            self.queue.push_back(Work::Erase(at, size));
            if let Some(c) = self.collection.as_mut() { c.erased += size.div_ceil(PAGE); }
        }
        Ok(())
    }
    fn begin_collect(&mut self, hw: Hw) -> Result<(), i32> {
        let l = self.layout;
        let plan = self.collection.as_ref().ok_or(ERR_ARG)?;
        if plan.done { return Ok(()); }
        if (plan.bytes != 0 && self.collection_space(hw)?.2 >= plan.bytes)
            || (!plan.full && plan.bytes == 0 && plan.processed != 0)
        {
            self.collection.as_mut().unwrap().done = true;
            return Ok(());
        }
        // Erase retired/orphan pages first, both for the target run and for copy workspace.
        if plan.erase {
            let used = self.collection_used(hw)?;
            for page in store::FRONT / PAGE..l.size / PAGE {
                if !used[page as usize] && !Collection::has(&plan.seen, page)
                    && !l.blank(hw, page * PAGE, PAGE)?
                {
                    let plan = self.collection.as_mut().unwrap();
                    Collection::mark(&mut plan.seen, page);
                    plan.processed += 1;
                    return self.reclaim(hw, page * PAGE, PAGE);
                }
            }
        }
        let plan = self.collection.as_ref().unwrap();
        let mut victim = None;
        l.areas(hw, |a| {
            if a.kind != b'D' || a.start < store::FRONT
                || Collection::has(&plan.seen, a.start / PAGE)
                || Collection::has(&plan.pinned, a.start / PAGE)
            { return Ok(()); }
            let mut retained = 0u32;
            l.records(hw, a, |_, t, n| {
                if t & 0x50 == 0x10 && (1..=3).contains(&(t & 15)) && t & 32 != 0 {
                    retained += 4 + store::align(n);
                }
                Ok(())
            })?;
            if victim.is_none_or(|(_, bytes)| retained < bytes) { victim = Some((a, retained)); }
            Ok(())
        })?;
        let Some((source, retained)) = victim else {
            let plan = self.collection.as_mut().unwrap();
            plan.done = true;
            return if plan.bytes != 0 { Err(NOSPACE) } else { Ok(()) };
        };
        let plan = self.collection.as_mut().unwrap();
        Collection::mark(&mut plan.seen, source.start / PAGE);
        plan.processed += 1;
        if retained == 0 { return self.reclaim(hw, source.start, PAGE); }
        self.collect = Some(Collect { source, dest: plan.dest, after: source.start + 16 });
        Ok(())
    }
    // Marker writes follow image validation and precede the durable operation completion.
    fn runtime_marker(&mut self, hw: Hw) -> Result<(), i32> {
        let l = self.layout;
        if self.record == 0 {
            let mut second = false;
            l.areas(hw, |a| {
                second |= a.kind == 0xe9 && a.start != 0;
                Ok(())
            })?;
            let at = store::align(self.size);
            let mut marker = [0; 4];
            l.read(hw, at, &mut marker)?;
            marker[0] &= if second { 0x78 } else { 0xf8 };
            self.program(at, &marker);
        } else if let Some(size) = l.image_size(hw, 0)? {
            let at = store::align(size);
            if at + 4 <= l.size {
                let mut word = [0; 4];
                l.read(hw, at, &mut word)?;
                if (0xf0..=0xfe).contains(&word[0]) {
                    word[0] &= 0x7f;
                    self.program(at, &word);
                }
            }
        }
        Ok(())
    }
    fn retire(&mut self, hw: Hw, source: store::Area) -> Result<(), i32> {
        if source.kind == 0xe9 {
            self.queue.push_back(Work::Erase(source.start, PAGE));
            for page in (source.start + PAGE..source.end).step_by(PAGE as usize) {
                self.queue.push_back(Work::Erase(page, PAGE));
            }
        } else {
            let mut h = [0; 4];
            self.layout.read(hw, source.start, &mut h)?;
            h[0] = 0;
            self.program(source.start, &h);
            for page in (source.start..source.end).step_by(PAGE as usize).rev() {
                self.queue.push_back(Work::Erase(page, PAGE));
            }
        }
        if matches!(source.kind, b'M' | 0xe9) {
            self.queue.push_back(Work::Rescan);
        }
        Ok(())
    }
    fn collect_step(&mut self, hw: Hw) -> Result<(), i32> {
        let mut c = self.collect.take().unwrap();
        let l = self.layout;
        let mut next = None;
        l.files(hw, |f| {
            if f.at >= c.after
                && f.at < c.source.end
                && f.live
                && !f.cache
                && next.is_none_or(|n: File| f.at < n.at)
            {
                next = Some(f);
            }
            Ok(())
        })?;
        let Some(f) = next else {
            if let Some(plan) = self.collection.as_mut() {
                plan.dest = c.dest;
                return self.reclaim(hw, c.source.start, c.source.end - c.source.start);
            }
            return self.retire(hw, c.source);
        };
        c.after = f.at + 4 + store::align(f.size + 12);
        if l.lookup(hw, f.name)?
            .is_none_or(|current| current.at != f.at)
        {
            self.collect = Some(c);
            return Ok(());
        }
        if c.dest == 0 || c.dest % PAGE == 0
            || c.dest + 4 + store::align(f.size + 12) > ((c.dest / PAGE) + 1) * PAGE
        {
            if self.collection.is_none() { return Err(NOSPACE); }
            let page = self.collection_free_page(hw)?;
            Collection::mark(&mut self.collection.as_mut().unwrap().seen, page / PAGE);
            let mut header = [255; 16];
            header[0] = b'D';
            header[4..8].copy_from_slice(&PAGE.to_le_bytes());
            self.program(page, &header);
            c.dest = page + 16;
        }
        if self.index == 0 || !l.blank(hw, self.index, 16)? {
            if self.collection.is_none() { return Err(NOSPACE); }
            self.index_slot(hw, c.dest / PAGE * PAGE)?;
        }
        let at = c.dest;
        self.program(at, &word(0xf0 | f.kind, f.size + 12));
        self.queue
            .push_back(Work::Copy(f.at + 4, at + 4, store::align(f.size + 12)));
        let mut moved = f;
        moved.at = at;
        let mut e = entry(moved);
        e[15] |= 128;
        self.program(self.index, &e[..12]);
        self.program(self.index + 12, &e[12..]);
        e[15] &= !128;
        self.program(self.index + 12, &e[12..]);
        self.index += 16;
        self.program(at, &word(0xb0 | f.kind, f.size + 12));
        c.dest += 4 + store::align(f.size + 12);
        self.collect = Some(c);
        Ok(())
    }
    fn protect(&self, hw: Hw, at: u32, size: u32) -> Result<(), i32> {
        let Some((running, len)) = crate::running_partition(hw) else {
            return Err(ERR_UNSUPPORTED);
        };
        let dest = self.layout.base + at;
        if dest < running.saturating_add(len)
            && running < dest.saturating_add(size.div_ceil(PAGE) * PAGE)
        {
            return Err(ERR_BUSY);
        }
        Ok(())
    }
    fn image_valid(&self, hw: Hw) -> Result<bool, i32> {
        if self.method == 14 {
            if !self.layout.verify_runtime(hw, self.record, self.size)? {
                return Ok(false);
            }
            let mut version = [0; 32];
            self.layout.read(hw, self.record + 48, &mut version)?;
            let mut v = 0u32;
            for b in version {
                if !b.is_ascii_digit() {
                    break;
                }
                v = v.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            return Ok(v == self.version);
        }
        let mut h = [0; 64];
        self.layout.read(hw, self.record, &mut h)?;
        if h[0] != b'M'
            || u16::from_le_bytes([h[2], h[3]]) != 64
            || store::u32le(&h[4..]) != self.size
        {
            return Ok(false);
        }
        let at = store::u32le(&h[44..]);
        let len = store::u32le(&h[48..]);
        if at < 64 || at > self.size || len > self.size - at {
            return Ok(false);
        }
        let mut m = [0; 48];
        let n = (len as usize).min(m.len());
        self.layout.read(hw, self.record + at, &mut m[..n])?;
        Ok(
            crate::manifest_ident(&m[..n]).is_some_and(|(name, version)| {
                name == self.name.split(|v| *v == 0).next().unwrap_or(&[])
                    && version == self.version
            }),
        )
    }
    fn submit(&mut self, hw: Hw, at: u32, data: &[u8]) -> Result<(), i32> {
        let p = hw.pkt_alloc()?;
        let (ptr, cap) = match hw.pkt_buf(p) {
            Ok(v) => v,
            Err(e) => {
                hw.pkt_free(p);
                return Err(e);
            }
        };
        if data.len() > cap {
            hw.pkt_free(p);
            return Err(ERR_SIZE);
        }
        unsafe {
            ptr.copy_from_nonoverlapping(data.as_ptr(), data.len());
            if self.method == 13 && at == self.record && data.first() == Some(&b'M') {
                *ptr = 0x4f;
            }
        }
        self.job = self.job.wrapping_add(1);
        let rc = hw.flash_write(
            self.layout.base,
            at,
            p,
            xip_abi::PKT_HEADROOM,
            data.len() as u32,
            44,
            EVENT,
            self.job,
        );
        if rc < 0 {
            hw.pkt_free(p);
            return Err(rc);
        }
        self.waiting = true;
        Ok(())
    }
    fn data(&mut self, hw: Hw, data: &[u8]) -> Result<usize, i32> {
        if !matches!(self.method, 4 | 10 | 13 | 14) {
            return if data.is_empty() { Ok(0) } else { Err(ERR_ARG) };
        }
        if self.fin || data.len() as u64 > u64::from(self.size - self.received) {
            return Err(ERR_SIZE);
        }
        if self.skip_body {
            self.received += data.len() as u32;
            return Ok(data.len());
        }
        if self.waiting || !self.queue.is_empty() || self.build.is_some() {
            return Ok(0);
        }
        if data.is_empty() {
            return Ok(0);
        }
        if self.carried != 0 || data.len() < 4 {
            let n = data.len().min(4 - self.carried);
            let mut carry = self.carry;
            carry[self.carried..self.carried + n].copy_from_slice(&data[..n]);
            if self.carried + n == 4 {
                self.submit(hw, self.at, &carry)?;
                self.at += 4;
                self.carried = 0;
                self.carry = [255; 4];
            } else {
                self.carry = carry;
                self.carried += n;
            }
            self.received += n as u32;
            return Ok(n);
        }
        let n = (data.len().min(PKT_MAX as usize)) & !3;
        self.submit(hw, self.at, &data[..n])?;
        self.at += n as u32;
        self.received += n as u32;
        Ok(n)
    }
    #[inline(never)]
    fn commit(&mut self, hw: Hw) -> Result<(), i32> {
        if self.committing {
            return Ok(());
        }
        self.committing = true;
        if self.method == 4 {
            if self.received != self.size {
                return Err(ERR_SIZE);
            }
            if self.carried != 0 {
                let at = self.at;
                let carry = self.carry;
                self.program(at, &carry);
                self.carried = 0;
            }
            let f = File {
                name: self.name,
                version: self.version,
                size: self.size,
                at: self.record,
                kind: self.kind,
                cache: self.cache,
                live: true,
            };
            let mut e = entry(f);
            e[15] |= 128;
            self.program(self.index, &e[..12]);
            self.program(self.index + 12, &e[12..]);
            e[15] &= !128;
            self.program(self.index + 12, &e[12..]);
            self.program(
                self.record,
                &word(
                    0x80 | 16 | if self.cache { 0 } else { 32 } | self.kind,
                    self.size + 12,
                ),
            );
            self.removing = true;
        } else if self.method == 10 {
            if self.received != self.size {
                return Err(ERR_SIZE);
            }
            if self.carried != 0 {
                let carry = self.carry;
                self.program(self.at, &carry);
                self.carried = 0;
            }
            self.program(self.record, &word(0xb5, self.size + 10));
        } else if matches!(self.method, 13 | 14) {
            if !self.skip_body && self.received != self.size {
                return Err(ERR_SIZE);
            }
            if self.carried != 0 {
                let at = self.at;
                let carry = self.carry;
                self.program(at, &carry);
                self.carried = 0;
            }
            // Validation is deferred until the last body/padding write completed.
            return Ok(());
        } else if self.method == 5 {
            self.removing = true;
        }
        Ok(())
    }
    fn remove_step(&mut self, hw: Hw) -> Result<(), i32> {
        let mut oldest = None;
        self.layout.files(hw, |f| {
            if f.live
                && f.name == self.name
                && (self.method != 4 || f.at != self.record)
                && oldest.is_none_or(|o: File| (f.version, f.at) < (o.version, o.at))
            {
                oldest = Some(f);
            }
            Ok(())
        })?;
        if let Some(f) = oldest {
            self.program(
                f.at,
                &word(0x80 | if f.cache { 0 } else { 32 } | f.kind, f.size + 12),
            );
        } else {
            self.removing = false;
        }
        Ok(())
    }
    fn erase_logs_step(&mut self, hw: Hw) -> Result<(), i32> {
        let through = self.erasing_logs.unwrap();
        let l = self.layout;
        let mut first = None;
        l.logs(hw, |b| {
            if b.last() <= through && first.is_none_or(|f: store::Batch| b.first < f.first) {
                first = Some(b);
            }
            Ok(())
        })?;
        let Some(batch) = first else {
            self.erasing_logs = None;
            return Ok(());
        };
        let mut host = None;
        l.areas(hw, |a| {
            if batch.at >= a.start && batch.at < a.end {
                host = Some(a);
            }
            Ok(())
        })?;
        let host = host.ok_or(ERR_IO)?;
        let mut all = host.kind == b'D';
        l.records(hw, host, |at, t, n| {
            if t & 0x50 != 0x10 {
                return Ok(());
            }
            if t & 15 != 5 || n < 10 {
                all = false;
                return Ok(());
            }
            let mut b = [0; 10];
            l.read(hw, at + 4, &mut b)?;
            let seq = u64::from_le_bytes(b[..8].try_into().unwrap());
            let count = u16::from_le_bytes([b[8], b[9]]);
            if count != 0 && seq.saturating_add(count as u64 - 1) > through {
                all = false;
            }
            Ok(())
        })?;
        if all {
            self.queue.push_back(Work::Erase(host.start, PAGE));
        } else {
            self.program(batch.at, &word(0xa5, batch.size + 10));
        }
        Ok(())
    }
    #[inline(never)]
    fn pump(&mut self, hw: Hw) -> Result<(), i32> {
        if self.waiting {
            return Ok(());
        }
        if let Some(work) = self.queue.pop_front() {
            let rc = match &work {
                Work::Copy(src, dest, len) => {
                    let n = (*len).min(PKT_MAX & !3);
                    let mut b = [255; PKT_MAX as usize];
                    self.layout.read(hw, *src, &mut b[..n as usize])?;
                    let rc = self.submit(hw, *dest, &b[..n as usize]);
                    if rc.is_ok() && *len > n {
                        self.queue
                            .push_front(Work::Copy(src + n, dest + n, len - n));
                    }
                    rc
                }
                Work::Rescan => {
                    let rc = hw.module_rescan();
                    if rc < 0 && rc != ERR_UNSUPPORTED {
                        Err(rc)
                    } else {
                        Ok(())
                    }
                }
                Work::CommitModule(at, b) => {
                    let method = self.method;
                    self.method = 0;
                    let rc = self.submit(hw, *at, b);
                    self.method = method;
                    rc
                }
                Work::Program(at, b) => self.submit(hw, *at, b),
                Work::Erase(at, n) => {
                    self.job += 1;
                    let rc = hw.flash_erase(self.layout.base, *at, *n, 44, EVENT, self.job);
                    if rc < 0 {
                        Err(rc)
                    } else {
                        self.waiting = true;
                        Ok(())
                    }
                }
            };
            if matches!(rc, Err(ERR_BUSY | ERR_NOMEM)) {
                self.queue.push_front(work);
                return Ok(());
            }
            return rc;
        }
        if let Some(mut b) = self.build.take() {
            if b.index < b.count {
                let f = self.layout.scan_next(hw, b.after)?.ok_or(ERR_IO)?;
                let e = entry(f);
                self.program(b.page + 24 + 16 * b.index as u32, &e);
                b.after = Some(f.name);
                b.index += 1;
                self.build = Some(b);
            } else {
                self.program(b.page + 16, &word(0xb4, 4 + 16 * (b.count as u32 + 256)));
                for page in b.old {
                    self.reclaim(hw, page, PAGE)?;
                }
            }
            return Ok(());
        }
        if let Some(mut pages) = self.sweep.take() {
            let page = pages
                .iter()
                .position(|p| *p == 2)
                .or_else(|| pages.iter().rposition(|p| *p == 1));
            if let Some(page) = page {
                pages[page] = 0;
                self.sweep = Some(pages);
                if !self.layout.blank(hw, page as u32 * PAGE, PAGE)? {
                    self.queue.push_back(Work::Erase(page as u32 * PAGE, PAGE));
                }
                return Ok(());
            }
        }
        if self.collect.is_some() {
            self.collect_step(hw)?;
            return Ok(());
        }
        if self.collection.as_ref().is_some_and(|c| !c.done) {
            self.begin_collect(hw)?;
            return Ok(());
        }
        if !self.fin {
            if let Some(local) = self.local {
                if self.received < self.size {
                    let (ptr, _) = hw.pkt_buf(local.request.pkt)?;
                    let at = self.received as usize;
                    let data = unsafe {
                        core::slice::from_raw_parts(
                            ptr.add(local.body_at + at),
                            local.body_len - at,
                        )
                    };
                    self.data(hw, data)?;
                }
                if self.received == self.size {
                    self.fin = true;
                }
            }
            return Ok(());
        }
        if !self.committing {
            self.commit(hw)?;
            return Ok(());
        }
        if self.removing {
            self.remove_step(hw)?;
            return Ok(());
        }
        if self.erasing_logs.is_some() {
            self.erase_logs_step(hw)?;
            return Ok(());
        }
        if self.error == 0 && matches!(self.method, 13 | 14) && !self.image_checked {
            self.image_checked = true;
            {
                if self.method == 13 && !self.skip_body {
                    let mut h = [0; 4];
                    self.layout.read(hw, self.record, &mut h)?;
                    if h[0] != 0x4f {
                        return Err(ERR_ARG);
                    }
                    h[0] = b'M';
                    // Validate the body identity with the pending lead temporarily accepted by the reader.
                    let mut header = [0; 64];
                    self.layout.read(hw, self.record, &mut header)?;
                    if store::u32le(&header[4..]) != self.size {
                        return Err(ERR_ARG);
                    }
                    let at = store::u32le(&header[44..]);
                    let len = store::u32le(&header[48..]);
                    if at < 64 || at > self.size || len > self.size - at || len == 0 {
                        return Err(ERR_ARG);
                    }
                    let mut m = [0; 48];
                    let n = (len as usize).min(48);
                    self.layout.read(hw, self.record + at, &mut m[..n])?;
                    if !crate::manifest_ident(&m[..n]).is_some_and(|(name, v)| {
                        name == self.name.split(|v| *v == 0).next().unwrap_or(&[])
                            && v == self.version
                    }) {
                        return Err(ERR_ARG);
                    }
                    // submit() normally keeps M pending, so enqueue the commit as a distinct work item below.
                    self.queue.push_back(Work::CommitModule(self.record, h));
                } else if !self.skip_body && !self.image_valid(hw)? {
                    return Err(ERR_ARG);
                }
                if self.method == 14 {
                    self.runtime_marker(hw)?;
                }

                if self.method == 13 {
                    self.queue.push_back(Work::Rescan);
                }
                return Ok(());
            }
        }
        if self.error == 0 && matches!(self.method, 13 | 14) {
            self.upload = None; // All body, marker, commit and rescan work has completed.
        }
        if self.method == 0 {
            self.reply = true;
            return Ok(());
        }
        self.output(hw)
    }
    fn output(&mut self, hw: Hw) -> Result<(), i32> {
        if self.done {
            return Ok(());
        }
        if self.out == 0 {
            if self.reply && self.read_len == 0 {
                if self.method != 11 {
                    return Ok(());
                }
                if let Some(b) = self.batch.take() {
                    self.since = b.last();
                    self.next_batch(hw)?;
                    self.reply = false;
                } else {
                    return Ok(());
                }
            }
            let p = hw.pkt_alloc()?;
            self.out = p;
            let (ptr, cap) = hw.pkt_buf(p)?;
            let buf = unsafe { core::slice::from_raw_parts_mut(ptr, cap) };
            let n = if !self.reply {
                let mut w = cbor::Writer::new(buf);
                if self.error < 0 {
                    if self.local.is_some() {
                        w.int(self.error as i64);
                    } else {
                        w.map(1).uint(7).int(self.error as i64);
                    }
                } else if self.local.is_some_and(|l| l.request.method == 17) {
                    let mut data = [0; LOCAL_MAX as usize];
                    if self.read_len != 0 {
                        self.layout
                            .read(hw, self.read_at, &mut data[..self.read_len as usize])?;
                    }
                    let offset = self.read_at - self.old.unwrap().at - 16;
                    w.map(6)
                        .uint(1)
                        .uint(self.version as u64)
                        .uint(2)
                        .uint(self.size as u64)
                        .uint(3)
                        .uint(self.kind as u64)
                        .uint(4)
                        .boolean(self.cache)
                        .uint(6)
                        .uint(offset as u64)
                        .uint(13)
                        .bytes(&data[..self.read_len as usize]);
                } else if self.local.is_some_and(|l| l.request.method == 20) {
                    self.local_logs(hw, &mut w)?;
                } else if self.method == 9 {
                    let (free, dirty, largest) = self.collection_space(hw)?;
                    let plan = self.collection.as_ref().ok_or(ERR_ARG)?;
                    w.map(5).uint(1).uint(free as u64).uint(2).uint(dirty as u64)
                        .uint(3).uint(largest as u64).uint(4).uint(plan.processed as u64)
                        .uint(5).uint(plan.erased as u64);
                } else if matches!(self.method, 13 | 14) {
                    w.map(3)
                        .uint(1)
                        .uint(self.version as u64)
                        .uint(2)
                        .uint(self.size as u64)
                        .uint(6)
                        .uint(self.record as u64);
                } else if matches!(self.method, 2 | 3 | 4) {
                    w.map(if self.method == 3 { 6 } else { 4 })
                        .uint(1)
                        .uint(self.version as u64)
                        .uint(2)
                        .uint(self.size as u64)
                        .uint(3)
                        .uint(self.kind as u64)
                        .uint(4)
                        .boolean(self.cache);
                    if self.method == 3 {
                        w.uint(5)
                            .uint(self.read_len as u64)
                            .uint(6)
                            .uint((self.read_at - self.old.unwrap().at - 16) as u64);
                    }
                } else if self.method == 6 {
                    let mut after = self.list_after;
                    let mut count = 0;
                    for _ in 0..self.size {
                        if let Some(f) = self.layout.next(hw, after)? {
                            after = Some(f.name);
                            count += 1;
                        } else {
                            break;
                        }
                    }
                    let more = self.layout.next(hw, after)?.is_some();
                    w.map(2).uint(1).array(count);
                    after = self.list_after;
                    for _ in 0..count {
                        let f = self.layout.next(hw, after)?.ok_or(ERR_IO)?;
                        w.map(5)
                            .uint(1)
                            .bytes(&f.name)
                            .uint(2)
                            .uint(f.version as u64)
                            .uint(3)
                            .uint(f.size as u64)
                            .uint(4)
                            .uint(f.kind as u64)
                            .uint(5)
                            .boolean(f.cache);
                        after = Some(f.name);
                    }
                    w.uint(2).boolean(more);
                } else if self.method == 23 {
                    crate::inventory::write(hw, self.layout, self.at, &mut w)?;
                } else if self.method == 8 {
                    let used = self.layout.occupied(hw)?;
                    let (mut free, mut dirty, mut run, mut largest) = (0, 0, 0, 0);
                    for p in store::FRONT / PAGE..self.layout.size / PAGE {
                        if !used[p as usize] {
                            if self.layout.blank(hw, p * PAGE, PAGE)? {
                                free += 1;
                                run += 1;largest = largest.max(run);
                            } else {
                                run = 0;dirty += 1;
                            }
                        } else { run = 0; }
                    }
                    w.map(if self.local.is_some() { 5 } else { 4 });
                    if self.local.is_some() {
                        w.uint(4).uint(LOCAL_MAX as u64);
                    }
                    w.uint(1)
                        .uint(free)
                        .uint(2)
                        .uint(dirty)
                        .uint(3)
                        .uint(MAX_FILE as u64).uint(5).uint((largest * PAGE) as u64);
                } else if self.method == 10 {
                    w.map(1)
                        .uint(1)
                        .uint(self.first_seq + self.count as u64 - 1);
                } else if self.method == 11 {
                    if let Some(b) = self.batch {
                        w.map(3)
                            .uint(1)
                            .uint(b.first)
                            .uint(2)
                            .uint(b.count as u64)
                            .uint(3)
                            .uint(b.size as u64);
                    } else {
                        w.map(0);
                    }
                } else {
                    w.map(0);
                }
                w.finish().ok_or(ERR_SIZE)?.len()
            } else {
                let n = self.read_len.min(PKT_MAX) as usize;
                self.layout.read(hw, self.read_at, &mut buf[..n])?;
                n
            };
            self.out = p;
            self.out_len = n as u32;
            self.out_fin = (self.error < 0
                || self.read_len == 0
                || (self.reply && n as u32 == self.read_len)) as u32;
            if self.method == 11 && self.batch.is_some() {
                self.out_fin = 0;
            }
        }
        if self.done {
            return Ok(());
        }
        let rc = if let Some(local) = self.local {
            let packet = self.out;
            self.out = 0;
            let rc = hw.send(
                packet,
                self.out_len as usize,
                local.request.from,
                local.request.component,
                local.request.method,
                local.request.id,
            );
            if rc == ERR_BUSY && local.request.from >= xip_abi::ADDR_LINK {
                self.out = packet; // A blocked link retains this exact lease.
                return Ok(());
            }
            if matches!(rc, ERR_BUSY | ERR_NOMEM) {
                return Ok(()); // A module send consumed it; recreate the reply next tick.
            }
            self.done = true;
            rc
        } else if self.tcp {
            let (ptr, _) = hw.pkt_buf(self.out)?;
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    ptr.add(self.sent as usize),
                    (self.out_len - self.sent) as usize,
                )
            };
            let n = hw.socket_write(self.handle, bytes);
            if n == xip_abi::ERR_AGAIN {
                return Ok(());
            }
            if n < 0 {
                return Err(n);
            }
            self.sent += n as u32;
            if self.sent < self.out_len {
                return Ok(());
            }
            hw.pkt_free(self.out);
            self.sent = 0;
            self.done = self.out_fin != 0;
            0
        } else {
            let table = hw.stream().ok_or(ERR_UNSUPPORTED)?;
            unsafe {
                table.write_pkt.ok_or(ERR_UNSUPPORTED)?(
                    hw.ctx(),
                    self.handle,
                    self.out,
                    self.out_len,
                    self.out_fin,
                )
            }
        };
        if rc == ERR_BUSY || rc == ERR_NOMEM {
            return Ok(());
        }
        if rc < 0 {
            return Err(rc);
        }
        if self.reply {
            self.read_at += self.out_len;
            self.read_len -= self.out_len;
        } else {
            self.reply = true;
        }
        self.out = 0;
        Ok(())
    }
    fn fail(&mut self, hw: Hw, e: i32) {
        if self.out != 0 {
            hw.pkt_free(self.out);
            self.out = 0;
        }
        self.error = e;
        self.queue.clear();
        self.build = None;
        self.collect = None;
        self.collection = None;
        self.removing = false;
        self.sweep = None;
        self.erasing_logs = None;
        self.fin = true;
        self.committing = true;
        self.reply = false;
        self.read_len = 0;
        self.batch = None;
    }
    fn dispose(&mut self, hw: Hw) {
        if let Some(local) = self.local.take() {
            hw.pkt_free(local.request.pkt);
        }
        if self.out != 0 {
            hw.pkt_free(self.out);
            self.out = 0;
        }
    }
}
impl Files {
    #[inline(never)]
    pub fn maintenance(hw: Hw) -> Option<Self> {
        let l = Layout::discover(hw).ok()?;
        let mut used = [false; 256];
        let mut first = [false; 256];
        let mut front = None;
        let mut current_known = false;
        let running = crate::running_partition(hw);
        l.walk(hw, |a| {
            if matches!(a.kind, 0 | 0x4f) {
                return Ok(());
            }
            if a.kind == 0xe9 && !l.verify_runtime(hw, a.start, a.bytes)? {
                first[(a.start / PAGE) as usize] = true;
                return Ok(());
            }
            if a.start == 0 && a.kind == 0xe9 {
                front = Some(a);
            }
            if a.kind == 0xe9 && running.is_some_and(|(at, _)| at == l.base + a.start) {
                current_known = true;
            }
            for p in a.start / PAGE..a.end / PAGE {
                used[p as usize] = true;
            }
            Ok(())
        })
        .ok()?;
        if !current_known {
            if let Some((at, size)) = running {
                let end = at.saturating_add(size).min(l.base + l.size);
                let start = at.max(l.base);
                if start < end {
                    for p in (start - l.base) / PAGE..(end - l.base).div_ceil(PAGE) {
                        used[p as usize] = true;
                    }
                }
            }
        }
        // Running module pages remain pinned even if their header is damaged.
        if !crate::for_modules(hw, |_, _, live, page| {
            if live {
                if let Some(page) = page {
                    let abs = page.saturating_mul(PAGE as u64);
                    if abs >= l.base as u64 && abs < (l.base + l.size) as u64 {
                        used[((abs - l.base as u64) / PAGE as u64) as usize] = true;
                    }
                }
            }
        }) {
            return None;
        }
        let mut pages = [0; 256];
        for p in 0..(l.size / PAGE) as usize {
            if !used[p] {
                pages[p] = if first[p] { 2 } else { 1 };
            }
        }
        let call = CallView {
            component: 44,
            method: 7,
            value: STREAM_OPEN,
            flags: xip_abi::CALL_STREAM,
            data: &[0xa0],
            time_us: 0,
            pkt: 0,
            from: 0,
            id: 0,
            request_id: None,
            src: None,
        };
        let mut t = Transfer::open(hw, &call, None, false, None).ok()?;
        t.method = 0;
        t.fin = true;
        t.committing = true;
        if let Some(a) = front {
            let at = store::align(a.bytes);
            if at + 4 <= l.size {
                let mut marker = [0; 4];
                l.read(hw, at, &mut marker).ok()?;
                // The marker may occupy the first word of the page after the image.
                pages[(at / PAGE) as usize] = 0;
                if matches!(marker[0], 0xfe | 0xfc) {
                    let end = if marker[0] == 0xfe {
                        l.size
                    } else {
                        store::FRONT.min(l.size)
                    };
                    let start = (at + 4).div_ceil(4096) * 4096;
                    // Never erase a mapped module, even during initial formatting.
                    let mut pinned = [false; 256];
                    crate::for_modules(hw, |_, _, live, page| {
                        if live {
                            if let Some(page) = page {
                                let abs = page.saturating_mul(PAGE as u64);
                                if abs >= l.base as u64 && abs < (l.base + l.size) as u64 {
                                    let offset = (abs - l.base as u64) as u32;
                                    let mut h = [0; 8];
                                    let span = if l.read(hw, offset, &mut h).is_ok() && h[0] == b'M'
                                    {
                                        store::u32le(&h[4..])
                                            .div_ceil(PAGE)
                                            .max(1)
                                            .min((l.size - offset) / PAGE)
                                    } else {
                                        1
                                    };
                                    for p in offset / PAGE..offset / PAGE + span {
                                        pinned[p as usize] = true;
                                    }
                                }
                            }
                        }
                    });
                    if let Some((base, span)) = running.filter(|(base, _)| *base != l.base) {
                        let begin = base.max(l.base) - l.base;
                        let finish = base
                            .saturating_add(span)
                            .min(l.base + l.size)
                            .saturating_sub(l.base);
                        if begin < finish {
                            pinned[(begin / PAGE) as usize..finish.div_ceil(PAGE) as usize]
                                .fill(true);
                        }
                    }
                    let mut run = None;
                    for sector in (start..end).step_by(4096).chain(core::iter::once(end)) {
                        if sector == end || pinned[(sector / PAGE) as usize] {
                            if let Some(begin) = run.take() {
                                t.queue.push_back(Work::Erase(begin, sector - begin));
                            }
                        } else if run.is_none() {
                            run = Some(sector);
                        }
                    }
                    // Formatting is restartable: clear the marker only after all erases.
                    marker[0] = 0xf8;
                    t.program(at, &marker);
                    if end > start {
                        pages[(start / PAGE) as usize..end.div_ceil(PAGE) as usize].fill(0);
                    }
                }
            }
        }
        if pages.iter().any(|p| *p != 0) {
            t.sweep = Some(pages);
        }
        if t.queue.is_empty() && t.sweep.is_none() {
            return None;
        }
        Some(Self {
            retry: None,
            queued: VecDeque::new(),
            transfer: Some(t),
            cursor: None,
        })
    }
    pub fn with_cursor(cursor: u32) -> Self {
        Self {
            retry: None,
            queued: VecDeque::new(),
            transfer: None,
            cursor: (cursor >= store::FRONT).then_some(cursor),
        }
    }
    pub fn cursor(&self) -> u32 {
        self.cursor.unwrap_or(0)
    }
    pub fn tcp_done(&self) -> bool {
        self.transfer.as_ref().is_some_and(|t| t.done)
    }
    pub fn releasable(&self) -> bool {
        self.transfer.is_none() && self.retry.is_none() && self.queued.is_empty()
    }
    pub fn busy(&self) -> bool {
        self.transfer.is_some() || !self.queued.is_empty()
    }
    pub fn request(&mut self, hw: Hw, c: &CallView) -> i32 {
        if local_method(c.method).is_none() {
            return ERR_UNSUPPORTED;
        }
        if self.queued.len() >= 4 {
            return ERR_BUSY;
        }
        if c.data.len() > PKT_MAX as usize {
            return ERR_SIZE;
        }
        let pkt = match hw.pkt_alloc() {
            Ok(p) => p,
            Err(e) => return e,
        };
        let (ptr, _) = match hw.pkt_buf(pkt) {
            Ok(p) => p,
            Err(e) => {
                hw.pkt_free(pkt);
                return e;
            }
        };
        unsafe {
            ptr.copy_from_nonoverlapping(c.data.as_ptr(), c.data.len());
        }
        self.queued.push_back(Request {
            pkt,
            len: c.data.len(),
            from: c.from,
            component: c.component,
            method: c.method,
            id: c.id,
        });
        self.poll_local(hw);
        0
    }
    fn poll_local(&mut self, hw: Hw) {
        if self.transfer.is_none() {
            if let Some(request) = self.queued.pop_front() {
                match Transfer::open_local(hw, request, self.cursor, self.retry) {
                    Ok(t) => self.transfer = Some(t),
                    Err(e) => {
                        let mut out = [0; 9];
                        let mut w = cbor::Writer::new(&mut out);
                        w.int(e as i64);
                        if let Some(b) = w.finish() {
                            hw.message(
                                request.from,
                                request.component,
                                request.method,
                                request.id,
                                b,
                            );
                        }
                        hw.pkt_free(request.pkt);
                    }
                }
            }
        }
    }
    #[inline(never)]
    pub fn handle(&mut self, hw: Hw, c: &CallView) -> i32 {
        self.handle_transport(hw, c, false)
    }
    pub fn handle_tcp(&mut self, hw: Hw, c: &CallView) -> i32 {
        self.handle_transport(hw, c, true)
    }
    #[inline(never)]
    fn handle_transport(&mut self, hw: Hw, c: &CallView, tcp: bool) -> i32 {
        self.poll_local(hw);
        if c.flags & xip_abi::CALL_STREAM != 0 {
            if c.value == STREAM_OPEN {
                if self.transfer.is_some() {
                    return ERR_BUSY;
                }
                match Transfer::open(hw, c, self.cursor, tcp, self.retry) {
                    Ok(t) => {
                        if matches!(c.method, 13 | 14) {
                            self.retry = None;
                        }
                        self.transfer = Some(t);
                    }
                    Err(e) => return e,
                };
            } else {
                let Some(t) = self.transfer.as_mut() else {
                    return ERR_NOTFOUND;
                };
                if t.handle != c.id as u32 {
                    return ERR_NOTFOUND;
                }
                match c.value {
                    STREAM_DATA => {
                        return match t.data(hw, c.data) {
                            Ok(n) => n as i32,
                            Err(ERR_BUSY | ERR_NOMEM) => 0,
                            Err(e) => e,
                        }
                    }
                    STREAM_FIN => t.fin = true,
                    STREAM_CLOSED => {
                        if t.upload.is_some() {
                            self.retry = t.upload;
                        }
                        if matches!(t.method, 4 | 10 | 13 | 14) && t.record >= store::FRONT {
                            self.cursor = Some(t.record / PAGE * PAGE);
                        }
                        t.dispose(hw);
                        self.transfer = None;
                        return 0;
                    }
                    _ => return ERR_ARG,
                }
            }
        } else if c.method == EVENT {
            if let Some(t) = self.transfer.as_mut() {
                if t.waiting && c.id == t.job {
                    t.waiting = false;
                    if c.value < 0 {
                        t.fail(hw, c.value);
                    }
                }
            }
        }
        if let Some(t) = self.transfer.as_mut() {
            if let Err(e) = t.pump(hw) {
                if !matches!(e, ERR_BUSY | ERR_NOMEM) {
                    t.fail(hw, e);
                }
            }
        }
        if self
            .transfer
            .as_ref()
            .is_some_and(|t| t.local.is_some() && t.done)
        {
            let mut t = self.transfer.take().unwrap();
            if matches!(t.method, 4 | 10) && t.record >= store::FRONT {
                self.cursor = Some(t.record / PAGE * PAGE);
            }
            t.dispose(hw);
        }
        if self
            .transfer
            .as_ref()
            .is_some_and(|t| t.method == 0 && t.reply)
        {
            self.transfer = None;
        }
        0
    }
    pub fn stop(&mut self, hw: Hw) {
        for request in self.queued.drain(..) {
            hw.pkt_free(request.pkt);
        }
        if let Some(t) = self.transfer.as_mut() {
            t.dispose(hw);
        }
        self.transfer = None;
    }
}
