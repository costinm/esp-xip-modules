//! Optional XIP adapter for the loader's existing signed TCP upload protocol.
//! No ESP-IDF or FreeRTOS dependency. All transfer state belongs to one connection.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
use alloc::vec::Vec;
use core::ptr;
#[cfg(any(target_arch = "riscv32", target_arch = "xtensa"))]
mod rtc;
mod state;
mod upload;
mod crypto;
mod wire;
// Retain the old blocking adapter only for protocol/interoperability tests.
#[cfg(test)]
mod device;
#[cfg(test)]
mod device_tests;
#[cfg(test)]
mod upload_tests;
#[cfg(test)]
mod psa_test;
use state::{FlashOps, StateLog};
use upload::{Action, Auth, Policy, Session};
use wire::{target, Reject};
use xip_abi::{
    cbor, CallView, Hw, Module, ERR_AGAIN, ERR_BUSY, ERR_NOMEM, HOST_START, HOST_STOP,
};
/// The component number of this module (`API.md`; `tools/mkmod.py` checks it against the manifest).
const COMPONENT: i32 = 44;
/// The loader scans the module partition (`data`) on pages of this size; a page that starts with the image header is a module.
const PAGE: u32 = 65536;
/// The method of the socket events of this module's port (the runtime listens on its TCP port: the manifest port, `--stream 2` = TCP 5227).
const IO: i32 = 0;
const FLASH: i32 = 101;
const TICK: i32 = 102;
#[cfg(feature = "esp32")]
const CPU: u8 = 0;
#[cfg(feature = "esp32s3")]
const CPU: u8 = 9;
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
const CPU: u8 = 13;
#[derive(Clone, Copy)]
struct Layout {
    loader: u32,
    loader_size: u32,
    data: u32,
    data_size: u32,
    main: u32,
    main_size: u32,
}
#[inline(always)]
fn layout(hw: Hw) -> Result<Layout, Reject> {
    let mut l = Layout {
        loader: 0,
        loader_size: 0,
        data: 0,
        data_size: 0,
        main: 0,
        main_size: 0,
    };
    for i in 0..96 {
        let mut b = [0u8; 32];
        if hw.flash_read(0x8000, i * 32, &mut b) != 32 {
            return Err(wire::FLASH);
        }
        if b[..2] == [0xff, 0xff] || b[..2] == [0xeb, 0xeb] {
            break;
        }
        if b[..2] != [0xaa, 0x50] {
            return Err(wire::WRONG_TARGET);
        }
        let addr = u32::from_le_bytes(b[4..8].try_into().unwrap());
        let size = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let n = b[12..28].iter().position(|x| *x == 0).unwrap_or(16);
        let name = &b[12..12 + n];
        if name == b"loader" && b[2] == 0 {
            l.loader = addr;
            l.loader_size = size;
        }
        if name == b"main" && b[2] == 0 {
            l.main = addr;
            l.main_size = size;
        }
        if name == b"data" && b[2] == 1 {
            l.data = addr;
            l.data_size = size;
        }
    }
    if l.loader != 0x10000
        || l.loader_size < 8192
        || l.data_size == 0
        || l.loader.checked_add(l.loader_size).is_none()
        || l.data.checked_add(l.data_size).is_none()
    {
        return Err(wire::WRONG_TARGET);
    }
    Ok(l)
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
/// The page of the module partition that holds the module `name` (and its component number), else the first page that
/// is not a module (the loader's rule: a page that starts with the image header is a module) with number 0.
/// The page number also indexes the module version entries of the loader state journal (eight, as the partition has eight pages).
fn module_page(hw: Hw, l: &Layout, name: &[u8]) -> Result<(usize, u16), Reject> {
    let mut free = None;
    for page in 0..(l.data_size / PAGE).min(state::MODULE_SLOTS as u32) {
        let mut h = [0u8; 64];
        if hw.flash_read(l.data, page * PAGE, &mut h) != 64 {
            return Err(wire::FLASH);
        }
        if h[..4] != *b"DMOD" {
            free.get_or_insert(page as usize);
            continue;
        }
        // the manifest starts {1: name, ...}
        let (at, len) = (le32(&h[44..48]), le32(&h[48..52]) as usize);
        let mut m = [0u8; 48];
        let n = len.min(m.len());
        if at < 64 || hw.flash_read(l.data, page * PAGE + at, &mut m[..n]) != n as i32 {
            continue;
        }
        let mut r = cbor::Reader::new(&m[..n]);
        if let (Some(cbor::Item::Map(_)), Some(cbor::Item::Uint(1)), Some(cbor::Item::Text(t))) = (r.next(), r.next(), r.next()) {
            if t == name {
                return Ok((page as usize, u16::from_le_bytes([h[16], h[17]])));
            }
        }
    }
    free.map(|p| (p, 0)).ok_or(wire::WRONG_TARGET)
}
/// `f(name, component, running)` for every module the host lists; false when the list cannot be read.
fn for_modules(hw: Hw, mut f: impl FnMut(&[u8], u64, bool)) -> bool {
    let mut bytes = [0u8; 1536];
    let n = hw.modules_info(&mut bytes);
    if n < 0 {
        return false;
    }
    let mut r = cbor::Reader::new(&bytes[..n as usize]);
    let Some(cbor::Item::Array(count)) = r.next() else {
        return false;
    };
    for _ in 0..count {
        let Some(cbor::Item::Map(fields)) = r.next() else {
            return false;
        };
        let (mut tag, mut running, mut name) = (0, false, &[][..]);
        for _ in 0..fields {
            let Some(cbor::Item::Uint(key)) = r.next() else {
                return false;
            };
            match (key, r.next()) {
                (1, Some(cbor::Item::Text(v))) => name = v,
                (2, Some(cbor::Item::Uint(v))) => tag = v,
                (4, Some(cbor::Item::Bool(v))) => running = v,
                (1 | 2 | 4, _) => return false,
                (_, Some(_)) => {}
                _ => return false,
            }
        }
        f(name, tag, running);
    }
    true
}
/// The request text of `start flash [REQUEST]` (not persistent: a reboot starts it strict again, `--autostart` has no request):
/// empty = signed by the pinned key; `unsigned` = signatures are not checked; 130 hex digits = a public key (uncompressed
/// P-256 point) accepted instead of the pinned key. For development.
fn parse_auth(request: &[u8]) -> Auth {
    if request == b"unsigned" {
        return Auth::Unsigned;
    }
    if request.len() == 130 {
        let mut key = [0u8; 65];
        for (i, b) in key.iter_mut().enumerate() {
            let d = |c: u8| (c as char).to_digit(16);
            match (d(request[2 * i]), d(request[2 * i + 1])) {
                (Some(h), Some(l)) => *b = (h * 16 + l) as u8,
                _ => return Auth::Strict,
            }
        }
        return Auth::Key(key);
    }
    Auth::Strict
}
struct Operation {
    offset: u32,
    data: Vec<u8>,
    erase: bool,
}
struct Journal {
    hw: Hw,
    base: u32,
    ops: Vec<Operation>,
}
impl FlashOps for Journal {
    fn read(&mut self, off: u32, out: &mut [u8]) -> bool {
        if off as usize + out.len() > 8192
            || self.hw.flash_read(self.base, off, out) != out.len() as i32
        {
            return false;
        }
        for op in &self.ops {
            let len = if op.erase { 4096 } else { op.data.len() };
            let begin = (off as usize).max(op.offset as usize);
            let end = (off as usize + out.len()).min(op.offset as usize + len);
            if begin < end {
                if op.erase {
                    out[begin - off as usize..end - off as usize].fill(0xff);
                } else {
                    for at in begin..end {
                        out[at - off as usize] &= op.data[at - op.offset as usize];
                    }
                }
            }
        }
        true
    }
    fn write(&mut self, off: u32, data: &[u8]) -> bool {
        if self.ops.try_reserve(1).is_err() {
            return false;
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(data.len()).is_err() {
            return false;
        }
        copy.extend_from_slice(data);
        self.ops.push(Operation {
            offset: off,
            data: copy,
            erase: false,
        });
        true
    }
    fn erase(&mut self, off: u32) -> bool {
        if self.ops.try_reserve(1).is_err() {
            return false;
        }
        self.ops.push(Operation {
            offset: off,
            data: Vec::new(),
            erase: true,
        });
        true
    }
}
struct Rules<'a> {
    view: &'a state::View,
    auth: Auth,
    hw: Hw,
    layout: Layout,
}
impl Policy for Rules<'_> {
    fn cpu(&self) -> u8 {
        CPU
    }
    fn pinned_key(&self) -> Option<[u8; 65]> {
        self.view.pin
    }
    fn require_pinned_key(&self) -> bool {
        true
    }
    fn auth(&self) -> Auth {
        self.auth
    }
    #[inline(always)]
    fn min_version(&self, t: u8, name: &[u8]) -> Result<Option<u32>, Reject> {
        Ok(match t {
            target::FLASH => None,
            target::LOADER => self.view.minver_loader,
            target::STAGE2 => self.view.minver_stage2,
            // the journal keeps eight module entries: one per page of the module partition
            target::MODULE => {
                let (page, _) = module_page(self.hw, &self.layout, name)?;
                self.view.minver_module[page]
            }
            _ => self.view.minver,
        })
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Work {
    None,
    Stop,
    Erase,
    Body,
    Commit,
    Head,
}
struct Reply {
    bytes: [u8; 96],
    len: usize,
    at: usize,
}
impl Reply {
    fn text(&mut self, s: &[u8]) {
        let n = s.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&s[..n]);
        self.len += n;
    }
    fn number(&mut self, mut n: u32) {
        let mut digits = [0u8; 10];
        let mut at = 10;
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.text(&digits[at..]);
    }
    fn ok(&mut self, size: u32, dry: bool) {
        self.text(b"OK ");
        self.number(size);
        if dry {
            self.text(b" dry");
        }
        self.text(b"\n");
    }
}
struct Connection {
    socket: u32,
    session: Session,
    log: StateLog<Journal>,
    layout: Layout,
    work: Work,
    waiting: bool,
    job: u64,
    base: u32,
    size: u32,
    target: u8,
    origin: u32,
    signed_header: [u8; wire::HEADER_SIZE],
    fed: usize,
    page: usize,
    tag: u16,
    name: [u8; 32],
    name_len: usize,
    auth: Auth,
    dry: bool,
    write: *const u8,
    write_len: usize,
    at: usize,
    op: usize,
    rx: u32,
    rx_at: usize,
    rx_len: usize,
    reply: Reply,
    reboot: bool,
    handoff: bool,
    last: u64,
    trace_at: u64,
}
impl Connection {
    #[inline(always)]
    fn new(hw: Hw, socket: u32, auth: Auth) -> Result<Self, Reject> {
        let l = layout(hw)?;
        Ok(Self {
            socket,
            session: Session::new(crate::crypto::Psa::from_host(hw).ok_or(wire::CRYPTO)?)?,
            log: StateLog::open(Journal {
                hw,
                base: l.loader + l.loader_size - 8192,
                ops: Vec::new(),
            }),
            layout: l,
            work: Work::None,
            waiting: false,
            job: 0,
            base: 0,
            size: 0,
            target: 0,
            origin: 0,
            signed_header: [0; wire::HEADER_SIZE],
            fed: 0,
            page: 0,
            tag: 0,
            name: [0; 32],
            name_len: 0,
            auth,
            dry: false,
            write: ptr::null(),
            write_len: 0,
            at: 0,
            op: 0,
            rx: 0,
            rx_at: 0,
            rx_len: 0,
            reply: Reply {
                bytes: [0; 96],
                len: 0,
                at: 0,
            },
            reboot: false,
            handoff: false,
            last: hw.now_ms(),
            trace_at: hw.now_ms(),
        })
    }
    fn fail(&mut self, r: Reject) {
        self.work = Work::None;
        self.waiting = false;
        self.reply.len = 0;
        self.reply.at = 0;
        self.reply.text(b"ERR ");
        self.reply.number(r.code as u32);
        self.reply.text(b" ");
        self.reply.text(r.reason.as_bytes());
        self.reply.text(b"\n");
    }
    #[inline(always)]
    fn open(&mut self, hw: Hw, t: u8, name: &[u8], size: u32, dry: bool) -> Result<(), Reject> {
        self.target = t;
        self.size = size;
        self.dry = dry;
        let (base, cap) = match t {
            target::MAIN => (self.layout.main, self.layout.main_size),
            target::FLASH => {
                let name = name.strip_prefix(b"0x").unwrap_or(name);
                if name.is_empty() || name.len() > 8 { return Err(wire::WRONG_TARGET); }
                let mut address = 0u32;
                for b in name {
                    let digit = match b { b'0'..=b'9' => b-b'0', b'a'..=b'f' => b-b'a'+10,
                        b'A'..=b'F' => b-b'A'+10, _ => return Err(wire::WRONG_TARGET) };
                    address = (address << 4) | u32::from(digit);
                }
                (address, u32::MAX-address)
            }
            target::LOADER => (self.layout.loader, self.layout.loader_size - 8192),
            target::STAGE2 => {
                let b = if CPU == 0 { 0x1000 } else { 0 };
                (b, 0x8000 - b)
            }
            target::MODULE => {
                let (page, tag) = module_page(hw, &self.layout, name)?;
                if tag == COMPONENT as u16 || name.len() > self.name.len() {
                    return Err(wire::WRONG_TARGET);
                }
                self.page = page;
                self.tag = tag;
                self.name[..name.len()].copy_from_slice(name);
                self.name_len = name.len();
                (self.layout.data + page as u32 * PAGE, PAGE)
            }
            _ => return Err(wire::WRONG_TARGET),
        };
        let prefix = if t == target::FLASH { base & 4095 } else { 0 };
        let erased = size.checked_add(prefix).and_then(|n| n.checked_add(4095))
            .ok_or(wire::BAD_LENGTH)? & !4095;
        if erased.saturating_sub(prefix) > cap {
            return Err(wire::BAD_LENGTH);
        }
        self.base = base;
        self.origin = base;
        if dry {
            self.session.opened(true)?;
        } else if t == target::FLASH {
            // Raw writes are coordinated by the owner/control plane.
            self.work = Work::Erase;
        } else {
            // a module is stopped before its page is erased; Main, the loader and Stage2 need every other module stopped
            let mut failed = false;
            let listed = for_modules(hw, |name, tag, _| {
                if tag != COMPONENT as u64 && (t != target::MODULE || name == &self.name[..self.name_len]) {
                    let rc = hw.module_stop(name);
                    failed |= rc < 0 && rc != xip_abi::ERR_NOTFOUND;
                }
            });
            if failed || !listed {
                return Err(wire::FLASH);
            }
            self.work = Work::Stop;
        }
        Ok(())
    }
    fn stopped(&self, hw: Hw) -> bool {
        let mut busy = false;
        let listed = for_modules(hw, |name, tag, running| {
            busy |= running
                && tag != COMPONENT as u64
                && (self.target != target::MODULE || name == &self.name[..self.name_len]);
        });
        listed && !busy
    }
    #[inline(always)]
    fn commit(&mut self, t: u8, version: u32, key: &[u8; 65]) -> Result<(), Reject> {
        // the key is pinned (trust on first use) only when packages are checked against the pinned key
        let ok = (self.auth != Auth::Strict || self.log.view.pin.is_some() || self.log.append(state::REC_PIN, key))
            && match t {
                target::FLASH => true,
                target::MAIN => self.log.append_u32(state::REC_MINVER, version)
                    && self.log.append(state::REC_HEADER, &self.signed_header),
                target::LOADER => self.log.append_u32(state::REC_MINVER_LOADER, version),
                target::STAGE2 => self.log.append_u32(state::REC_MINVER_STAGE2, version),
                target::MODULE => self.log.append_module_version(self.page as u8, version),
                _ => false,
            };
        if !ok {
            return Err(wire::STATE_WRITE);
        }
        self.base = self.origin;
        self.op = 0;
        self.at = 0;
        self.work = Work::Commit;
        Ok(())
    }
    fn write_packet(hw: Hw, base: u32, off: u32, bytes: &[u8], job: u64) -> Result<usize, i32> {
        let pkt = hw.pkt_alloc()?;
        let (ptr, cap) = match hw.pkt_buf(pkt) {
            Ok(v) => v,
            Err(e) => {
                hw.pkt_free(pkt);
                return Err(e);
            }
        };
        let n = bytes.len().min(cap);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, n);
        }
        let start = hw
            .pkt_info()
            .map(|v| v.0)
            .unwrap_or(xip_abi::PKT_HEADROOM);
        let rc = hw.flash_write(base, off, pkt, start, n as u32, COMPONENT, FLASH, job);
        if rc < 0 {
            hw.pkt_free(pkt);
            Err(rc)
        } else {
            Ok(n)
        }
    }
    #[inline(always)]
    fn pump_work(&mut self, hw: Hw) -> Result<(), Reject> {
        if self.waiting {
            return Ok(());
        }
        self.job = self.job.wrapping_add(1);
        match self.work {
            Work::Stop => {
                if self.stopped(hw) {
                    self.work = Work::Erase;
                }
                return Ok(());
            }
            Work::Erase => {
                let rc = hw.flash_erase(
                    self.origin & !4095,
                    0,
                    (self.size + (self.origin & 4095) + 4095) & !4095,
                    COMPONENT,
                    FLASH,
                    self.job,
                );
                if rc == ERR_BUSY {
                    return Ok(());
                }
                if rc < 0 {
                    return Err(wire::FLASH);
                }
                self.waiting = true;
            }
            Work::Body | Work::Head => {
                if self.at == self.write_len {
                    self.session.written(true)?;
                    self.work = Work::None;
                    // base had been temporarily moved to the block address.
                    if let Some((_, size)) = self.session.done() {
                        self.reply.ok(size, false);
                        self.reboot = matches!(self.target, target::MAIN | target::LOADER | target::STAGE2);
                    }
                } else {
                    if self.work == Work::Head && self.target == target::MODULE && self.at == 0 {
                        let bytes =
                            unsafe { core::slice::from_raw_parts(self.write, self.write_len) };
                        if bytes.len() < 64
                            || bytes[..4] != [0x44, 0x4d, 0x4f, 0x44]
                            || u16::from_le_bytes([bytes[6], bytes[7]]) != 64
                            || u32::from_le_bytes(bytes[12..16].try_into().unwrap()) != self.size
                            || u16::from_le_bytes([bytes[16], bytes[17]]) == COMPONENT as u16
                            || (self.tag != 0 && u16::from_le_bytes([bytes[16], bytes[17]]) != self.tag)
                        {
                            return Err(wire::BAD_IMAGE_HEADER);
                        }
                    }
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            self.write.add(self.at),
                            self.write_len - self.at,
                        )
                    };
                    match Self::write_packet(hw, self.base, self.at as u32, bytes, self.job) {
                        Ok(n) => {
                            self.at += n;
                            self.waiting = true;
                        }
                        Err(ERR_BUSY | ERR_NOMEM) => {}
                        Err(_) => return Err(wire::FLASH),
                    }
                }
            }
            Work::Commit => {
                if self.op == self.log.ops().ops.len() {
                    self.log.ops().ops.clear();
                    let action = self.session.committed(true)?;
                    if let Action::Write { index: _, bytes } = action {
                        self.write = bytes.as_ptr();
                        self.write_len = bytes.len();
                        self.at = 0;
                        self.work = Work::Head;
                    }
                } else {
                    let op = &self.log.ops().ops[self.op];
                    let base = self.layout.loader + self.layout.loader_size - 8192;
                    if op.erase {
                        let rc = hw.flash_erase(base, op.offset, 4096, COMPONENT, FLASH, self.job);
                        if rc == ERR_BUSY {
                            return Ok(());
                        }
                        if rc < 0 {
                            return Err(wire::STATE_WRITE);
                        }
                        self.waiting = true;
                        self.op += 1;
                    } else if self.at == op.data.len() {
                        self.op += 1;
                        self.at = 0;
                    } else {
                        match Self::write_packet(
                            hw,
                            base,
                            op.offset + self.at as u32,
                            &op.data[self.at..],
                            self.job,
                        ) {
                            Ok(n) => {
                                self.at += n;
                                self.waiting = true;
                            }
                            Err(ERR_BUSY | ERR_NOMEM) => {}
                            Err(_) => return Err(wire::STATE_WRITE),
                        }
                    }
                }
            }
            Work::None => {}
        }
        Ok(())
    }
    #[inline(always)]
    fn input(&mut self, hw: Hw) -> Result<(), Reject> {
        if self.reply.len != 0 || self.work != Work::None || self.waiting {
            return Ok(());
        }
        if self.rx == 0 {
            self.rx = match hw.pkt_alloc() {
                Ok(p) => p,
                Err(ERR_BUSY | ERR_NOMEM) => return Ok(()),
                Err(_) => return Err(wire::NO_MEMORY),
            };
        }
        let (ptr, cap) = hw.pkt_buf(self.rx).map_err(|_| wire::NO_MEMORY)?;
        if self.rx_at == self.rx_len {
            self.rx_at = 0;
            self.rx_len = 0;
            let n = hw.socket_read(self.socket, unsafe {
                core::slice::from_raw_parts_mut(ptr, cap)
            });
            if n == ERR_AGAIN {
                return Ok(());
            }
            if n < 0 {
                return Err(wire::SHORT_READ);
            }
            if n == 0 {
                let action = self.session.eof()?;
                match action {
                    Action::Commit {
                        target,
                        name,
                        version,
                        pubkey,
                    } => {
                        let mut nm = [0; 128];
                        let len = name.len();
                        nm[..len].copy_from_slice(name);
                        let key = *pubkey;
                        self.commit(target, version, &key)?;
                    }
                    Action::RequestLoader { dry } => {
                        self.reply.text(b"OK 0 request-loader");
                        if dry {
                            self.reply.text(b" dry");
                        }
                        self.reply.text(b"\n");
                        self.handoff = !dry;
                        self.reboot = !dry;
                    }
                    Action::Done { image_size, .. } => {
                        self.reply.ok(image_size, true);
                    }
                    _ => return Err(wire::SHORT_READ),
                }
                return Ok(());
            }
            self.rx_len = n as usize;
            self.last = hw.now_ms();
        }
        let step = self.session.feed(
            unsafe { core::slice::from_raw_parts(ptr.add(self.rx_at), self.rx_len - self.rx_at) },
            &Rules {
                view: &self.log.view,
                auth: self.auth,
                hw,
                layout: self.layout,
            },
        )?;
        // Retain the authenticated header for the existing Main state record.
        // Capture only bytes consumed by Session; no new public parser API.
        let header_at = wire::TRANSPORT_SIZE + wire::PUBKEY_SIZE + wire::SIG_SIZE;
        let start = self.fed.max(header_at);
        let end = (self.fed + step.consumed).min(header_at + wire::HEADER_SIZE);
        if end > start {
            let from = self.rx_at + start - self.fed;
            let bytes = unsafe { core::slice::from_raw_parts(ptr.add(from), end-start) };
            self.signed_header[start-header_at..end-header_at].copy_from_slice(bytes);
        }
        self.fed += step.consumed;
        self.rx_at += step.consumed;
        match step.action {
            Some(Action::Open {
                target,
                name,
                image_size,
                dry,
            }) => {
                let mut nm = [0; 128];
                let len = name.len();
                nm[..len].copy_from_slice(name);
                self.open(hw, target, &nm[..len], image_size, dry)?;
            }
            Some(Action::Write { index, bytes }) => {
                self.write = bytes.as_ptr();
                self.write_len = bytes.len();
                self.at = 0;
                self.work = Work::Body;
                // Offset is computed from the immutable destination each time.
                self.base = self.origin + index * 4096;
            }
            _ => {
                if step.consumed == 0 && self.rx_at < self.rx_len {
                    return Err(wire::BAD_LENGTH);
                }
            }
        }
        Ok(())
    }
    fn trace(&mut self, hw: Hw) {
        let now = hw.now_ms();
        if now.saturating_sub(self.trace_at) < 5000 { return; }
        self.trace_at = now;
        let mut text = *b"upload phase=0 waiting=0 fed=00000000 base=00000000 at=00000000 job=00000000";
        text[13] = b'0' + self.work as u8;
        text[23] = if self.waiting { b'1' } else { b'0' };
        for (start, value) in [(29,self.fed as u32),(43,self.base),(55,self.at as u32),(68,self.job as u32)] {
            for k in 0..8 { text[start+k] = b"0123456789abcdef"[((value >> (28-k*4)) & 15) as usize]; }
        }
        hw.log(&text);
    }
    #[inline(always)]
    fn advance(&mut self, hw: Hw) -> bool {
        self.trace(hw);
        if self.reply.len == 0 {
            if hw.now_ms().saturating_sub(self.last) > 30_000 {
                self.fail(wire::SHORT_READ);
            } else {
                // Drain immediately available parser/work transitions before
                // relying on another poll event. Readiness may already have
                // been delivered while a flash write was pending.
                for _ in 0..16 {
                    let before = (self.fed, self.work, self.at, self.op);
                    if let Err(r) = self.pump_work(hw).and_then(|_| self.input(hw)) {
                        self.fail(r);
                        break;
                    }
                    if self.waiting || self.reply.len != 0
                        || before == (self.fed, self.work, self.at, self.op) { break; }
                }
            }
        }
        if self.reply.len != 0 {
            let n = hw.socket_write(
                self.socket,
                &self.reply.bytes[self.reply.at..self.reply.len],
            );
            if n == ERR_AGAIN {
                return false;
            }
            if n < 0 {
                return true;
            }
            self.reply.at += n as usize;
            if self.reply.at == self.reply.len {
                if self.reboot {
                    if self.handoff {
                        arm_loader();
                    }
                    hw.reset();
                }
                return true;
            }
        }
        false
    }
}
fn arm_loader() {
    #[cfg(any(target_arch = "riscv32", target_arch = "xtensa"))]
    unsafe {
        let high = match CPU {
            0 => 0x3ff82000,
            9 => 0x60100000,
            _ => 0x50004000,
        };
        core::ptr::write_volatile(
            (high - rtc::RETAIN_RAW_SIZE + rtc::HANDOFF_OFFSET) as *mut u8,
            rtc::HANDOFF_LOADER,
        );
    }
}
#[repr(C)]
pub struct State {
    /// Started: the runtime listens on this module's port from the moment it is created, there is no listener to open.
    started: bool,
    connection: *mut Connection,
    auth: Auth,
    alarm: i32,
    status_published: bool,
    status_busy: bool,
}
impl State {
    fn publish_status(&mut self, hw: Hw) {
        let busy = !self.connection.is_null();
        if self.status_published && self.status_busy == busy { return; }
        let mut map = [0u8; 32];
        let mut w = cbor::Writer::new(&mut map);
        w.map(3).uint(1).uint(wire::PORT as u64).uint(2).boolean(self.started).uint(3).boolean(busy);
        if let Some(map) = w.finish() {
            if hw.info_publish(COMPONENT, 0, map) == 0 {
                self.status_published = true;
                self.status_busy = busy;
            }
        }
    }
    fn dispose(&mut self, hw: Hw) {
        if !self.connection.is_null() {
            unsafe {
                let c = &mut *self.connection;
                hw.socket_close(c.socket);
                if c.rx != 0 {
                    hw.pkt_free(c.rx);
                }
                ptr::drop_in_place(self.connection);
            }
            hw.mem_free(self.connection.cast());
            self.connection = ptr::null_mut();
        }
    }
}
impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_START) {
            if self.started {
                return ERR_BUSY;
            }
            self.alarm = hw.alarm_set(100, 0, 100, xip_abi::ALARM_EXACT, COMPONENT, TICK);
            if self.alarm < 0 {
                return self.alarm;
            }
            self.auth = parse_auth(call.data);
            self.started = true;
            self.publish_status(*hw);
            return 0;
        }
        if call.is_host(HOST_STOP) {
            self.dispose(*hw);
            if self.alarm > 0 {
                hw.alarm_cancel(self.alarm);
            }
            self.started = false;
            self.status_published = false;
            return 0;
        }
        if call.component == COMPONENT
            && call.method == IO
            && call.value == xip_abi::SOCKET_EVENT_OPEN as i32
        {
            if !self.connection.is_null() {
                hw.socket_close(call.id as u32);
                return 0;
            }
            let memory = hw
                .mem_alloc(core::mem::size_of::<Connection>())
                .cast::<Connection>();
            if memory.is_null() {
                hw.socket_close(call.id as u32);
                return 0;
            }
            match Connection::new(*hw, call.id as u32, self.auth) {
                Ok(c) => {
                    unsafe {
                        memory.write(c);
                    }
                    self.connection = memory;
                }
                Err(_) => {
                    hw.mem_free(memory.cast());
                    hw.socket_close(call.id as u32);
                }
            }
        }
        if !self.connection.is_null() {
            let c = unsafe { &mut *self.connection };
            if call.component == COMPONENT && call.method == FLASH && c.waiting && call.id == c.job
            {
                c.waiting = false;
                if call.value < 0 {
                    c.fail(wire::FLASH);
                } else if c.work == Work::Erase {
                    if c.session.opened(true).is_err() {
                        c.fail(wire::FLASH);
                    }
                    c.work = Work::None;
                }
            }
            if c.advance(*hw) {
                self.dispose(*hw);
            }
        }
        self.publish_status(*hw);
        0
    }
}

#[cfg(test)]
mod tests;
