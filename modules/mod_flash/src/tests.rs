use super::*;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use std::{collections::HashMap, vec, vec::Vec};
#[derive(Clone)]
struct Raw(Vec<u8>);
impl FlashOps for Raw {
    fn read(&mut self, off: u32, out: &mut [u8]) -> bool {
        out.copy_from_slice(&self.0[off as usize..off as usize + out.len()]);
        true
    }
    fn write(&mut self, off: u32, data: &[u8]) -> bool {
        for (i, b) in data.iter().enumerate() {
            self.0[off as usize + i] &= *b;
        }
        true
    }
    fn erase(&mut self, off: u32) -> bool {
        self.0[off as usize..off as usize + 4096].fill(255);
        true
    }
}
struct Fake {
    flash: Vec<u8>,
    input: Vec<u8>,
    at: usize,
    out: Vec<u8>,
    packets: HashMap<u32, Vec<u8>>,
    next: u32,
    job: Option<(u64, i32)>,
    busy_once: bool,
    writes: Vec<(u32, usize)>,
    read_size: usize,
    fail_commit: bool,
    /// the request of `start flash`
    auth: Auth,
    nv_auth: Option<Vec<u8>>,
    /// `device_info` says Main (0xf0000) is the running application, as when mod_flash runs inside Main
    running: bool,
    /// pages of running modules `modules_info` lists (key 12), each with a nested shadowed list
    running_pages: Vec<u8>,
    rescans: u32,
}
impl Fake {
    fn new(image: &[u8], target: u8, dry: bool) -> Self {
        Self::named(image, target, dry, b"test")
    }
    fn named(image: &[u8], target: u8, dry: bool, name: &[u8]) -> Self {
        Self::versioned(image, target, dry, name, 2)
    }
    /// A package of `name` at package version `version`.
    fn versioned(image: &[u8], target: u8, dry: bool, name: &[u8], version: u32) -> Self {
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        let pubkey: [u8; 65] = key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap();
        let mut state = StateLog::open(Raw(vec![255; 8192]));
        assert!(state.append(state::REC_PIN, &pubkey));
        let header = wire::Header::build(CPU, target, version, image, name);
        let sig: Signature = key.sign(&header);
        let mut input = wire::Transport::encode(dry, false).to_vec();
        input.extend_from_slice(&pubkey);
        input.extend_from_slice(&sig.to_bytes());
        input.extend_from_slice(&header);
        input.extend_from_slice(&wire::block_table(image));
        input.extend_from_slice(image);
        let mut flash = vec![255; 0x400000];
        for (index, (kind, name, base, size)) in [
            (0, b"loader".as_slice(), 0x10000u32, 0xe0000u32),
            (1, b"data".as_slice(), 0x380000u32, 0x80000u32),
            (0, b"main".as_slice(), 0xf0000u32, 0x290000u32),
        ]
        .iter()
        .enumerate()
        {
            let e = &mut flash[0x8000 + index * 32..0x8000 + (index + 1) * 32];
            e.fill(0);
            e[..2].copy_from_slice(&[0xaa, 0x50]);
            e[2] = *kind;
            e[4..8].copy_from_slice(&base.to_le_bytes());
            e[8..12].copy_from_slice(&size.to_le_bytes());
            e[12..12 + name.len()].copy_from_slice(name);
        }
        flash[0xee000..0xf0000].copy_from_slice(&state.ops().0);
        Self {
            flash,
            input,
            at: 0,
            out: Vec::new(),
            packets: HashMap::new(),
            next: 1,
            job: None,
            busy_once: true,
            writes: Vec::new(),
            read_size: 37,
            fail_commit: false,
            auth: Auth::Strict,
            nv_auth: None,
            running: false,
            running_pages: Vec::new(),
            rescans: 0,
        }
    }
    fn host(&mut self) -> xip_abi::Host {
        let mut h: xip_abi::Host = unsafe { core::mem::zeroed() };
        h.abi_version = xip_abi::ABI_VERSION;
        h.size = core::mem::size_of_val(&h) as u32;
        h.user = (self as *mut Self).cast();
        h.now_ms = Some(now);
        h.get_component = Some(crate::psa_test::get_component);
        h.flash_read = Some(read);
        h.flash_write = Some(write);
        h.flash_erase = Some(erase);
        h.pkt_alloc = Some(pkt_alloc);
        h.pkt_buf = Some(pkt_buf);
        h.pkt_free = Some(pkt_free);
        h.pkt_info = Some(pkt_info);
        h.socket_read = Some(socket_read);
        h.socket_write = Some(socket_write);
        h.module_stop = Some(stop);
        h.modules_info = Some(info);
        h.module_rescan = Some(rescan);
        h.device_info = Some(device_info);
        h.nv = Some(nv);
        h
    }
}
unsafe extern "C" fn nv(u: *mut core::ffi::c_void, key: *const u8, kn: usize,
    _: *const u8, _: usize, out: *mut u8, cap: usize) -> i32 {
    if core::slice::from_raw_parts(key, kn) != b"flash.auth" { return xip_abi::ERR_NOTFOUND; }
    match &f(u).nv_auth {
        Some(v) if v.len() <= cap => { core::ptr::copy_nonoverlapping(v.as_ptr(), out, v.len()); v.len() as i32 }
        Some(_) => xip_abi::ERR_SIZE,
        None => xip_abi::ERR_NOTFOUND,
    }
}
#[test]
fn activation_auth_uses_nv_and_explicit_requests_override_it() {
    let mut fake = Fake::new(&module_image(), target::MODULE, false);
    for (stored, expected) in [
        (None, Auth::Strict),
        (Some(b"strict".as_slice()), Auth::Strict),
        (Some(b"unsigned".as_slice()), Auth::Unsigned),
        (Some(b"garbage".as_slice()), Auth::Strict),
        (Some(b"unsigned trailing".as_slice()), Auth::Strict),
    ] {
        fake.nv_auth = stored.map(|v| v.to_vec());
        let h = fake.host();
        let hw = unsafe { Hw::new(&h) }.unwrap();
        assert_eq!(start_auth(hw, b""), expected);
        assert_eq!(start_auth(hw, b"strict"), Auth::Strict);
        assert_eq!(start_auth(hw, b"unsigned"), Auth::Unsigned);
        assert_eq!(start_auth(hw, b"invalid"), Auth::Strict);
    }
    // Exercise the real signature/hash acceptance path with the NV-selected mode.
    fake.nv_auth = Some(b"unsigned".to_vec());
    let h = fake.host();
    fake.auth = start_auth(unsafe { Hw::new(&h) }.unwrap(), b"");
    unsigned(&mut fake);
    let (_, done) = run(&mut fake);
    assert!(done && fake.out.starts_with(b"OK"));
}
unsafe fn f<'a>(u: *mut core::ffi::c_void) -> &'a mut Fake {
    &mut *(u as *mut Fake)
}
unsafe extern "C" fn now(_: *mut core::ffi::c_void) -> u64 {
    1
}
unsafe extern "C" fn read(u: *mut core::ffi::c_void, b: u32, o: u32, p: *mut u8, n: usize) -> i32 {
    let f = f(u);
    let a = (b + o) as usize;
    core::slice::from_raw_parts_mut(p, n).copy_from_slice(&f.flash[a..a + n]);
    n as i32
}
unsafe extern "C" fn erase(
    u: *mut core::ffi::c_void,
    b: u32,
    o: u32,
    n: u32,
    _: i32,
    _: i32,
    id: u64,
) -> i32 {
    let f = f(u);
    if f.busy_once {
        f.busy_once = false;
        return ERR_BUSY;
    }
    if f.job.is_some() {
        return ERR_BUSY;
    }
    f.flash[(b + o) as usize..(b + o + n) as usize].fill(255);
    f.job = Some((id, 0));
    0
}
unsafe extern "C" fn write(
    u: *mut core::ffi::c_void,
    b: u32,
    o: u32,
    pkt: u32,
    start: u32,
    n: u32,
    _: i32,
    _: i32,
    id: u64,
) -> i32 {
    let f = f(u);
    if f.job.is_some() {
        return ERR_BUSY;
    }
    let packet = f.packets.remove(&pkt).unwrap();
    let a = (b + o) as usize;
    let rc = if f.fail_commit && (0xee000..0xf0000).contains(&a) {
        xip_abi::ERR_IO
    } else {
        for i in 0..n as usize {
            f.flash[a + i] &= packet[start as usize + i];
        }
        f.writes.push((a as u32, n as usize));
        0
    };
    f.job = Some((id, rc));
    0
}
unsafe extern "C" fn pkt_alloc(u: *mut core::ffi::c_void, p: *mut u32) -> i32 {
    let f = f(u);
    *p = f.next;
    f.packets.insert(f.next, vec![0; 256]);
    f.next += 1;
    0
}
unsafe extern "C" fn pkt_buf(
    u: *mut core::ffi::c_void,
    p: u32,
    out: *mut *mut u8,
    cap: *mut usize,
) -> i32 {
    let f = f(u);
    let b = f.packets.get_mut(&p).unwrap();
    *out = b.as_mut_ptr();
    *cap = b.len();
    0
}
unsafe extern "C" fn pkt_free(u: *mut core::ffi::c_void, p: u32) -> i32 {
    f(u).packets.remove(&p);
    0
}
unsafe extern "C" fn pkt_info(_: *mut core::ffi::c_void, start: *mut u32, cap: *mut u32) -> i32 {
    *start = 0;
    *cap = 256;
    0
}
unsafe extern "C" fn socket_read(
    u: *mut core::ffi::c_void,
    _: u32,
    out: *mut u8,
    cap: usize,
) -> i32 {
    let f = f(u);
    if f.at == f.input.len() {
        return 0;
    }
    let n = cap.min(f.read_size).min(f.input.len() - f.at);
    core::slice::from_raw_parts_mut(out, n).copy_from_slice(&f.input[f.at..f.at + n]);
    f.at += n;
    n as i32
}
unsafe extern "C" fn socket_write(
    u: *mut core::ffi::c_void,
    _: u32,
    data: *const u8,
    n: usize,
) -> i32 {
    let f = f(u);
    let n = n.min(3);
    f.out
        .extend_from_slice(core::slice::from_raw_parts(data, n));
    n as i32
}
/// The running application: partition `main` at 0xf0000 (key 1 of the device info), as `status` shows it.
unsafe extern "C" fn device_info(u: *mut core::ffi::c_void, out: *mut u8, n: usize) -> i32 {
    if !f(u).running {
        return -1;
    }
    let b = core::slice::from_raw_parts_mut(out, n);
    let mut w = cbor::Writer::new(b);
    w.map(1).uint(1).map(3).uint(4).text("main").uint(5).uint(0xf0000).uint(6).uint(0x290000);
    w.finish().unwrap().len() as i32
}
unsafe extern "C" fn stop(_: *mut core::ffi::c_void, _: *const u8, _: usize) -> i32 {
    0
}
unsafe extern "C" fn info(u: *mut core::ffi::c_void, out: *mut u8, n: usize) -> i32 {
    let b = core::slice::from_raw_parts_mut(out, n);
    let mut w = cbor::Writer::new(b);
    w.array(1 + f(u).running_pages.len()).map(3).uint(1).text("test").uint(2).uint(2).uint(4).boolean(false);
    for &page in &f(u).running_pages {
        // a running module with the fields a newer loader adds: its page (12) and a nested list of shadowed images (13)
        w.map(5).uint(1).text("busy").uint(2).uint(50 + page as u64).uint(4).boolean(true).uint(12).uint(page as u64);
        w.uint(13).array(1).map(3).uint(1).uint(7).uint(2).uint(1).uint(3).boolean(false);
    }
    w.finish().unwrap().len() as i32
}
unsafe extern "C" fn rescan(u: *mut core::ffi::c_void) -> i32 {
    f(u).rescans += 1;
    1
}
fn run(f: &mut Fake) -> (Connection, bool) {
    let h = f.host();
    let hw = unsafe { Hw::new(&h) }.unwrap();
    let mut c = Connection::new(hw, 1, f.auth).unwrap();
    let mut done = false;
    for _ in 0..5000 {
        if let Some((id, rc)) = f.job.take() {
            assert_eq!(id, c.job);
            c.waiting = false;
            if rc < 0 {
                c.fail(wire::FLASH);
            } else if c.work == Work::Erase {
                c.session.opened(true).unwrap();
                c.work = Work::None;
            }
        }
        if c.advance(hw) {
            done = true;
            break;
        }
    }
    if c.rx != 0 {
        hw.pkt_free(c.rx);
        c.rx = 0;
    }
    assert!(f.packets.is_empty());
    (c, done)
}
fn module_image() -> Vec<u8> {
    let mut data = vec![42; 8192 + 333];
    data[..4].copy_from_slice(b"DMOD");
    data[6..8].copy_from_slice(&64u16.to_le_bytes());
    let size = data.len() as u32;
    data[12..16].copy_from_slice(&size.to_le_bytes());
    data[16..18].copy_from_slice(&2u16.to_le_bytes());
    data[44..52].fill(0); // no manifest
    data
}
#[test]
fn fragmented_module_upload_commits_state_before_block_zero() {
    let image = module_image();
    let mut f = Fake::new(&image, target::MODULE, false);
    let (_, done) = run(&mut f);
    assert!(done);
    assert_eq!(f.out, b"OK 8525\n");
    assert_eq!(&f.flash[0x380000..0x380000 + image.len()], &image);
    let first_head = f.writes.iter().position(|(a, _)| *a == 0x380000).unwrap();
    let last_state = f
        .writes
        .iter()
        .rposition(|(a, _)| (0xee000..0xf0000).contains(a))
        .unwrap();
    assert!(last_state < first_head);
    let persisted = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(persisted.view.module_floor(0), Some(2));
}
#[test]
fn bad_signature_and_dry_run_never_erase() {
    let image = module_image();
    let mut f = Fake::new(&image, target::MODULE, false);
    f.input[16 + 65] ^= 1;
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 7 "));
    assert!(f.writes.is_empty());
    assert!(f.flash[0x380000..0x390000].iter().all(|b| *b == 255));
    let mut f = Fake::new(&image, target::MODULE, true);
    run(&mut f);
    assert_eq!(f.out, b"OK 8525 dry\n");
    assert!(f.writes.is_empty());
    assert!(f.flash[0x380000..0x390000].iter().all(|b| *b == 255));
}
#[test]
fn failed_state_commit_never_writes_the_image_head() {
    let mut f = Fake::new(&module_image(), target::MODULE, false);
    f.fail_commit = true;
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 5 "));
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000));
}
#[test]
fn truncated_and_overlong_streams_never_commit() {
    let mut f = Fake::new(&module_image(), target::MODULE, false);
    f.input.truncate(f.input.len() - 1);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 6 "));
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000));
    let mut f = Fake::new(&module_image(), target::MODULE, false);
    f.input.push(99);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 3 "));
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000));
}
#[test]
fn loader_and_stage2_use_the_same_protocol_and_durable_versions() {
    for (target, base) in [
        (target::LOADER, 0x10000u32),
        (target::STAGE2, if CPU == 0 { 0x1000 } else { 0 }),
    ] {
        let mut image = vec![42; 4096 + 333];
        image[0] = 0xe9;
        image[12..14].copy_from_slice(&(CPU as u16).to_le_bytes());
        let mut f = Fake::new(&image, target, false);
        let (c, done) = run(&mut f);
        assert!(done);
        assert_eq!(f.out, b"OK 4429\n");
        assert!(c.reboot);
        assert_eq!(&f.flash[base as usize..base as usize + image.len()], &image);
        let head = f.writes.iter().position(|(a, _)| *a == base).unwrap();
        let state = f
            .writes
            .iter()
            .rposition(|(a, _)| (0xee000..0xf0000).contains(a))
            .unwrap();
        assert!(state < head);
        let log = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
        assert_eq!(
            if target == target::LOADER {
                log.view.minver_loader
            } else {
                log.view.minver_stage2
            },
            Some(2)
        );
    }
}
#[test]
fn malformed_module_header_is_never_made_executable() {
    let mut image = module_image();
    image[0] = 0;
    let mut f = Fake::new(&image, target::MODULE, false);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 4 "));
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000));
}
#[test]
fn the_running_application_is_never_erased() {
    // Main (partition 0xf0000..0x380000) and raw writes that touch it are refused when Main runs this module; elsewhere they are not
    let mut image = vec![42; 4096 + 333];
    image[0] = 0xe9;
    image[12..14].copy_from_slice(&(CPU as u16).to_le_bytes());
    let mut f = Fake::new(&image, target::MAIN, false);
    f.running = true;
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 10 "), "{:?}", f.out);
    assert!(f.writes.is_empty());
    for (address, ok) in [("000ff000", false), ("00001000", true), ("0037f000", false), ("0038f000", true)] {
        let mut f = Fake::named(&image, target::FLASH, false, address.as_bytes());
        f.running = true;
        run(&mut f);
        assert_eq!(f.out.starts_with(b"OK "), ok, "{address} {:?}", f.out);
    }
}
#[test]
fn main_image_commits_header_and_version_before_head_and_retains_loader_handoff() {
    let mut image = vec![42; 4096+333];
    image[0] = 0xe9;
    image[12..14].copy_from_slice(&(CPU as u16).to_le_bytes());
    let mut f = Fake::new(&image, target::MAIN, false);
    let expected = f.input[145..361].to_vec();
    let (c, done) = run(&mut f);
    assert!(done && c.reboot);
    assert_eq!(f.out, b"OK 4429\n");
    assert_eq!(&f.flash[0xf0000..0xf0000+image.len()], &image);
    let log = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(log.view.minver, Some(2));
    assert_eq!(log.view.header.as_ref().unwrap().as_slice(), expected);
    let state = f.writes.iter().rposition(|(a,_)| (0xee000..0xf0000).contains(a)).unwrap();
    let head = f.writes.iter().position(|(a,_)| *a==0xf0000).unwrap();
    assert!(state < head);
    let mut failed = Fake::new(&image, target::MAIN, false);
    failed.fail_commit = true;
    run(&mut failed);
    assert!(failed.out.starts_with(b"ERR 5 "));
    assert!(!failed.writes.iter().any(|(a,_)| *a==0xf0000));
    let mut f = Fake::new(&[], target::MAIN, false);
    let (c, done) = run(&mut f);
    assert!(done);
    assert_eq!(f.out, b"OK 0 request-loader\n");
    assert!(c.handoff);
    assert!(f.writes.is_empty());
}

#[test]
fn raw_flash_uses_the_signed_address_and_accepts_arbitrary_bytes() {
    let image = vec![0x41; 4096+19];
    // NVS, Main, a module and the Stage2 region: no partition policy applies.
    for address in [0u32, 0x9001, 0xf0000, 0x380001] {
        let name = std::format!("{address:08x}");
        let mut f = Fake::named(&image, target::FLASH, false, name.as_bytes());
        let (c, done) = run(&mut f);
        assert!(done && !c.reboot);
        assert_eq!(f.out, b"OK 4115\n");
        assert_eq!(&f.flash[address as usize..address as usize+image.len()], &image);
        assert!(!f.writes.iter().any(|(a,_)| (0xee000..0xf0000).contains(a)));
        assert!(f.packets.is_empty());
    }
    let mut dry = Fake::named(&image, target::FLASH, true, b"00009000");
    run(&mut dry);assert!(dry.writes.is_empty());
    assert_eq!(dry.out, b"OK 4115 dry\n");
    let mut invalid = Fake::named(&image, target::FLASH, false, b"not-hex");
    run(&mut invalid);assert!(invalid.out.starts_with(b"ERR 10 "));
    assert!(invalid.writes.is_empty());
    let mut signature = Fake::named(&image, target::FLASH, false, b"00009000");
    // Change the signed address without resigning it.
    signature.input[145+88+7] = b'f';
    run(&mut signature);assert!(signature.out.starts_with(b"ERR 7 "));
    assert!(signature.writes.is_empty());
}

#[test]
fn large_main_drains_buffered_input_between_async_completions() {
    let mut image = vec![42; 512 * 1024 + 17];
    image[0] = 0xe9;
    image[12..14].copy_from_slice(&(CPU as u16).to_le_bytes());
    let mut f = Fake::new(&image, target::MAIN, false);
    let (_, done) = run(&mut f);
    assert!(done, "buffered input must progress without waiting for a new readiness edge");
    assert_eq!(f.out, std::format!("OK {}\n", image.len()).as_bytes());
    assert_eq!(&f.flash[0xf0000..0xf0000 + image.len()], image.as_slice());
}

/// Install a fake module image (header and manifest `{1: name, 2: tag}`) on a page of the module partition.
fn module_on_page(f: &mut Fake, page: usize, name: &[u8], tag: u16) {
    let at = 0x380000 + page * 0x10000;
    f.flash[at..at + 4].copy_from_slice(b"DMOD");
    f.flash[at+12..at+16].copy_from_slice(&256u32.to_le_bytes());
    f.flash[at + 16..at + 18].copy_from_slice(&tag.to_le_bytes());
    f.flash[at + 44..at + 48].copy_from_slice(&64u32.to_le_bytes());
    let mut m = vec![0xa2, 1, 0x60 + name.len() as u8];
    m.extend_from_slice(name);
    m.extend_from_slice(&[2, tag as u8]);
    f.flash[at + 48..at + 52].copy_from_slice(&(m.len() as u32).to_le_bytes());
    f.flash[at + 64..at + 64 + m.len()].copy_from_slice(&m);
}
/// `module_on_page` for a module with a version (manifest `{1: name, 8: version, 2: tag}`).
fn module_v(f: &mut Fake, page: usize, name: &[u8], tag: u16, version: u8) {
    let at = 0x380000 + page * 0x10000;
    f.flash[at..at + 4].copy_from_slice(b"DMOD");
    f.flash[at+12..at+16].copy_from_slice(&256u32.to_le_bytes());
    f.flash[at + 16..at + 18].copy_from_slice(&tag.to_le_bytes());
    f.flash[at + 44..at + 48].copy_from_slice(&64u32.to_le_bytes());
    let mut m = vec![if version > 0 { 0xa3 } else { 0xa2 }, 1, 0x60 + name.len() as u8];
    m.extend_from_slice(name);
    if version > 0 {
        m.extend_from_slice(&[8, version]);
    }
    m.extend_from_slice(&[2, tag as u8]);
    f.flash[at + 48..at + 52].copy_from_slice(&(m.len() as u32).to_le_bytes());
    f.flash[at + 64..at + 64 + m.len()].copy_from_slice(&m);
}
fn written_page(f: &Fake, page: usize, image: &[u8]) -> bool {
    f.flash[0x380000 + page * 0x10000..][..image.len()] == *image
}
#[test]
fn a_module_goes_to_the_first_free_page_never_over_its_installed_version() {
    let image = module_image(); // tag 2, package version 2
    // pages 0 and 1 are other modules, "test" (version 1) lives on page 3: the new version takes the free page 2, page 3 stays
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 0, b"a", 5);
    module_on_page(&mut f, 1, b"b", 6);
    module_v(&mut f, 3, b"test", 2, 1);
    let old = f.flash[0x380000 + 3 * 0x10000..][..80].to_vec();
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    assert!(written_page(&f, 2, &image));
    assert_eq!(&f.flash[0x380000 + 3 * 0x10000..][..80], old.as_slice(), "the installed version is not touched");
    let persisted = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(persisted.view.module_floor(2), Some(2), "the journal entry follows the page of the image");
    assert_eq!(persisted.view.module_floor(3), None);
    assert_eq!(f.rescans, 1, "the loader is asked to look again after the commit");
    // a new module takes the first page that is not a module
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 0, b"a", 5);
    let (_, done) = run(&mut f);
    assert!(done);
    assert!(written_page(&f, 1, &image));
}
#[test]
fn an_equal_or_lower_version_is_refused_before_anything_is_erased() {
    let image = module_image();
    for installed in [2u8, 3, 9] {
        let mut f = Fake::new(&image, target::MODULE, false); // package version 2
        module_v(&mut f, 1, b"test", 2, installed);
        run(&mut f);
        assert!(f.out.starts_with(b"ERR 9 "), "installed {installed}: {:?}", f.out);
        assert!(f.writes.is_empty());
        assert_eq!(f.rescans, 0);
    }
    // the dry run says the same
    let mut f = Fake::new(&image, target::MODULE, true);
    module_v(&mut f, 1, b"test", 2, 2);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 9 "), "{:?}", f.out);
    // an installed version 0 (no key 8) is older than any version above 0, equal to 0
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 1, b"test", 2);
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 0);
    module_on_page(&mut f, 1, b"test", 2);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 9 "), "{:?}", f.out);
}
#[test]
fn with_no_free_page_the_oldest_shadowed_page_is_reused() {
    let image = module_image(); // "test" 2
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 5);
    // eight pages: test v4 active (page 6) over v1 (page 2) and v3 (page 4); b v7 (page 0) over b v2 (page 7); a, c alone
    module_v(&mut f, 0, b"b", 6, 7);
    module_v(&mut f, 1, b"a", 5, 1);
    module_v(&mut f, 2, b"test", 2, 1);
    module_v(&mut f, 3, b"c", 7, 1);
    module_v(&mut f, 4, b"test", 2, 3);
    module_v(&mut f, 5, b"d", 8, 1);
    module_v(&mut f, 6, b"test", 2, 4);
    module_v(&mut f, 7, b"b", 6, 2);
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    assert!(written_page(&f, 2, &image), "the lowest shadowed version (test v1) goes first");
    let persisted = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(persisted.view.module_floor(2), Some(5));
    // the next one: test v3 on page 4 is now the lowest shadowed version of all (b v2 is 2, below it)
    let pages = read_pages_of(&f);
    assert_eq!(choose_page(&pages, b"test", 6, 0).unwrap().0, 7, "b v2 (page 7) is the oldest shadowed image now");
}
#[test]
fn nothing_is_reused_when_every_page_is_active_or_running() {
    let image = module_image();
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 5);
    for p in 0..8 {
        module_v(&mut f, p, &[b'a' + p as u8], 20 + p as u16, 1);
    }
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 10 "), "{:?}", f.out);
    assert!(f.writes.is_empty());
    // shadowed pages of running modules are not offered: protected pages are skipped
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 5);
    for p in 0..8 {
        module_v(&mut f, p, if p < 4 { b"x" } else { b"y" }, 20, (p % 4) as u8 + 1);
    }
    let pages = read_pages_of(&f);
    // x: v1..v4 on pages 0..3, active page 3; y active page 7. Shadowed pages 0,1,2,4,5,6
    // oldest version first (v1 on pages 0 and 4: the lower page), the active pages 3 and 7 never
    assert_eq!(choose_page(&pages, b"new", 1, 0).unwrap().0, 0);
    assert_eq!(choose_page(&pages, b"new", 1, 1 << 0).unwrap().0, 4);
    assert_eq!(choose_page(&pages, b"new", 1, 1 << 0 | 1 << 4).unwrap().0, 1);
    assert_eq!(choose_page(&pages, b"new", 1, 1 << 0 | 1 << 4 | 1 << 1).unwrap().0, 5);
    assert_eq!(choose_page(&pages, b"new", 1, 0b0011_0011).unwrap().0, 2, "v3 is the newest shadowed one, still not the active v4");
    assert!(choose_page(&pages, b"new", 1, 0b0111_0111).is_err());
    // a running module's page comes from modules_info (key 12) and is protected
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 5);
    for p in 0..8 {
        module_v(&mut f, p, if p < 4 { b"x" } else { b"y" }, 20, (p % 4) as u8 + 1);
    }
    f.running_pages = vec![0, 1, 2];
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    assert!(written_page(&f, 4, &image), "pages 0-2 run: the oldest shadowed page left is 4 (v1); page 3 is active");
}
fn read_pages_of(f: &Fake) -> Pages {
    let mut pages: Pages = vec![None; 8];
    let mut f2 = Fake::new(&[1; 4], target::MODULE, true);
    f2.flash.copy_from_slice(&f.flash);
    let h = f2.host();
    let hw = unsafe { Hw::new(&h) }.unwrap();
    let l = layout(hw).unwrap();
    pages.copy_from_slice(&read_pages(hw, &l).unwrap());
    pages
}
#[test]
fn the_active_page_of_a_name_is_the_highest_version_then_the_lowest_page() {
    let image = module_image();
    let mut f = Fake::new(&image, target::MODULE, true);
    module_v(&mut f, 0, b"m", 9, 2);
    module_v(&mut f, 1, b"m", 9, 5);
    module_v(&mut f, 2, b"m", 9, 5);
    module_v(&mut f, 3, b"m", 9, 0);
    let pages = read_pages_of(&f);
    let active: Vec<bool> = (0..4).map(|p| is_active(&pages, p)).collect();
    assert_eq!(active, [false, true, false, false]);
    assert!(!is_active(&pages, 7), "a free page is nothing");
    assert_eq!(pages[1].unwrap().version, 5);
}
#[test]
fn the_package_and_its_image_say_the_same_name_and_version() {
    // Manifest {1: "test", 8: 2} in block zero, a later block, or the second flash page.
    let make = |size: usize, name: &[u8], version: u8| {
        let mut data = vec![42u8; size];
        data[..4].copy_from_slice(b"DMOD");
        data[6..8].copy_from_slice(&64u16.to_le_bytes());
        data[12..16].copy_from_slice(&(size as u32).to_le_bytes());
        data[16..18].copy_from_slice(&2u16.to_le_bytes());
        let mut m = vec![0xa2, 1, 0x60 + name.len() as u8];
        m.extend_from_slice(name);
        m.extend_from_slice(&[8, version]);
        let at = size - m.len() - size % 4;
        data[44..48].copy_from_slice(&(at as u32).to_le_bytes());
        data[48..52].copy_from_slice(&(m.len() as u32).to_le_bytes());
        data[at..at + m.len()].copy_from_slice(&m);
        data
    };
    for size in [300, 4096 + 700, 65536 + 700] {
        let image = make(size, b"test", 2);
        let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 2);
        let (_, done) = run(&mut f);
        assert!(done, "size {size}: {:?}", f.out);
        assert!(written_page(&f, 0, &image));
        for (what, image) in [("version", make(size, b"test", 3)), ("name", make(size, b"other", 2))] {
            let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 2);
            run(&mut f);
            assert!(f.out.starts_with(b"ERR 4 "), "size {size}, other {what}: {:?}", f.out);
            assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000), "block zero (the header) is never written: the page stays free");
            assert_eq!(f.rescans, 0);
        }
    }
}
#[test]
fn the_rollback_floor_of_a_name_is_the_highest_journal_entry_of_its_pages() {
    let image = module_image();
    let mut f = Fake::versioned(&image, target::MODULE, false, b"test", 3);
    module_v(&mut f, 0, b"test", 2, 1);
    module_v(&mut f, 1, b"test", 2, 2);
    // the journal says page 1 holds a package of version 4 (the log keeps the highest it was told)
    let mut state = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert!(state.append_module_version(1, 4));
    f.flash[0xee000..0xf0000].copy_from_slice(&state.ops().0);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 9 "), "{:?}", f.out);
}
#[test]
fn the_flasher_never_replaces_itself_or_a_module_with_another_number() {
    let image = module_image();
    // "test" is the flasher's own page (component 44)
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 1, b"test", 44);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 10 "), "{:?}", f.out);
    assert!(f.writes.is_empty());
    // the installed "test" has number 9, the package carries 2
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 1, b"test", 9);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 4 "), "{:?}", f.out);
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x380000 || *a == 0x390000));
}
/// An unsigned package: zero key and signature.
fn unsigned(f: &mut Fake) {
    for b in &mut f.input[16..16 + 65 + 64] {
        *b = 0;
    }
}
#[test]
fn development_modes_accept_unsigned_or_custom_key_packages_and_pin_nothing() {
    let image = module_image();
    // unsigned is refused by default, accepted with the setting; the hashes are still checked
    let mut f = Fake::new(&image, target::MODULE, false);
    unsigned(&mut f);
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 7 "));
    let mut f = Fake::new(&image, target::MODULE, false);
    f.auth = parse_auth(b"unsigned");
    unsigned(&mut f);
    f.flash[0xee000..0xf0000].fill(255); // no key pinned
    let (_, done) = run(&mut f);
    assert!(done, "{:?} {:x?}", f.out, f.writes.iter().map(|w| w.0).collect::<Vec<_>>());
    assert!(f.out.starts_with(b"OK"), "{}", std::string::String::from_utf8_lossy(&f.out)); assert_eq!(&f.flash[0x380000..][..image.len()], &image);
    assert_eq!(StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec())).view.pin, None);
    let mut f = Fake::new(&image, target::MODULE, false);
    f.auth = parse_auth(b"unsigned");
    unsigned(&mut f);
    *f.input.last_mut().unwrap() ^= 1;
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 14 ") || f.out.starts_with(b"ERR 13 "), "{:?}", f.out);
    // a custom key: the package key of the Fake is [7; 32]; the pinned key is not consulted
    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    let hex: std::string::String = key.verifying_key().to_encoded_point(false).as_bytes().iter().map(|b| std::format!("{b:02x}")).collect();
    let mut f = Fake::new(&image, target::MODULE, false);
    f.flash[0xee000..0xf0000].fill(255);
    f.auth = parse_auth(hex.as_bytes());
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    assert_eq!(StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec())).view.pin, None);
    let mut f = Fake::new(&image, target::MODULE, false);
    f.auth = parse_auth(hex.replace("04", "05").as_bytes()); // some other key
    run(&mut f);
    assert!(f.out.starts_with(b"ERR 8 "), "{:?}", f.out);
}
#[test]
fn the_manifest_options_in_cargo_toml_fit_the_module() {
    let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    assert!(toml.contains(&std::format!("--name flash --tag {COMPONENT} ")));
    let state: usize = toml.split("--state ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(core::mem::size_of::<State>() <= state, "the state block is {state} bytes, State needs {}", core::mem::size_of::<State>());
}

/// The frozen formats, against files made by the loader project's tools / the IDF (tests/data, docs/development.md "Formats").
#[test]
fn the_partition_table_of_stage2_is_read() {
    let table = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/partition-4mb.bin")).unwrap();
    let mut f = Fake::new(&module_image(), target::MODULE, false);
    f.flash[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    let h = f.host();
    let l = layout(unsafe { Hw::new(&h) }.unwrap()).unwrap();
    assert_eq!((l.loader, l.loader_size, l.main, l.main_size, l.data, l.data_size),
               (0x10000, 0xe0000, 0xf0000, 0x290000, 0x380000, 0x80000));
}
#[test]
fn a_signed_package_made_by_the_loader_tools_is_installed_over_the_socket_protocol() {
    // loader package of version 9, with the key it was signed with pinned
    let pkg = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/loader-esp32c6.pkg")).unwrap();
    let mut f = Fake::new(&module_image(), target::LOADER, false);
    let mut log = StateLog::open(Raw(vec![255; 8192]));
    assert!(log.append(state::REC_PIN, &pkg[..65]));
    f.flash[0xee000..0xf0000].copy_from_slice(&log.ops().0);
    f.input = wire::Transport::encode(false, false).to_vec();
    f.input.extend_from_slice(&pkg);
    let (c, done) = run(&mut f);
    assert!(done && c.reboot, "{:?}", std::string::String::from_utf8_lossy(&f.out));
    assert!(f.out.starts_with(b"OK ") && f.out.ends_with(b"\n"), "{:?}", f.out);
    let log = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(log.view.minver_loader, Some(9));
}

#[test]
fn multi_page_images_protect_tails_and_require_contiguous_space() {
    let image=module_image();let mut f=Fake::new(&image,target::MODULE,false);
    module_v(&mut f,0,b"nan",46,1);
    f.flash[0x380000+12..0x380000+16].copy_from_slice(&(65536u32+100).to_le_bytes());
    let pages=read_pages_of(&f);
    assert_eq!(pages[1].unwrap().first,0);
    assert!(is_active(&pages,0));assert!(!is_active(&pages,1));
    assert_eq!(choose_span(&pages,b"other",1,PageSet::low(0),2,8).unwrap().0,2);
    assert_eq!(choose_page(&pages,b"other",1,0).unwrap().0,2,"active continuation cannot be reused");
    assert!(choose_span(&pages,b"other",1,PageSet::low(0),2,2).is_err());
    assert!(choose_span(&pages,b"nan",1,PageSet::low(0),2,8).is_err(),"rollback still refused");
    module_v(&mut f,4,b"nan",46,2);let pages=read_pages_of(&f);
    assert_eq!(choose_span(&pages,b"other",1,PageSet::low(0),2,2).unwrap().0,0,"whole shadowed extent may be reused");
    assert!(choose_span(&pages,b"other",1,PageSet::low(1),2,2).is_err(),"running old head protects its complete extent");
}

#[test]
fn expanded_module_partition_admits_and_protects_high_pages() {
    let mut f=Fake::new(&module_image(),target::MODULE,false);
    // Same 4 MiB flash, with the C-only 1.5 MiB module partition.
    f.flash[0x8000+32+4..0x8000+32+8].copy_from_slice(&0x280000u32.to_le_bytes());
    f.flash[0x8000+32+8..0x8000+32+12].copy_from_slice(&0x180000u32.to_le_bytes());
    f.flash[0x8000+64+8..0x8000+64+12].copy_from_slice(&0x190000u32.to_le_bytes());
    for page in 0..20 {
        let name=std::format!("active{page}");module_v(&mut f,0,name.as_bytes(),2,1);
        let header=f.flash[0x380000..0x380000+256].to_vec();
        let at=0x280000+page*65536;f.flash[at..at+256].copy_from_slice(&header);
    }
    let h=f.host();let hw=unsafe {Hw::new(&h)}.unwrap();let l=layout(hw).unwrap();
    let pages=read_pages(hw,&l).unwrap();assert_eq!(pages.len(),24);
    assert_eq!(choose_span(&pages,b"quic",1,PageSet::default(),4,24).unwrap().0,20);
    module_v(&mut f,0,b"quic",48,1);let mut header=f.flash[0x380000..0x380000+256].to_vec();
    header[12..16].copy_from_slice(&(3*65536u32+100).to_le_bytes());
    f.flash[0x3c0000..0x3c0000+256].copy_from_slice(&header);
    module_v(&mut f,0,b"quic",48,2);let header=f.flash[0x380000..0x380000+256].to_vec();
    f.flash[0x280000..0x280000+256].copy_from_slice(&header);
    let pages=read_pages(hw,&l).unwrap();assert_eq!(pages[23].unwrap().first,20);
    assert_eq!(choose_span(&pages,b"other",1,PageSet::default(),4,24).unwrap().0,20);
    f.running_pages=vec![20];let protected=running_pages(hw).unwrap();
    assert!(choose_span(&pages,b"other",1,protected,4,24).is_err(),"running high-page head protects its entire four-page image");
    let mut journal=StateLog::open(Raw(vec![255;8192]));assert!(journal.append_module_version(20,77));
    assert_eq!(module_floor(hw,&l,&journal.view,b"quic").unwrap(),Some(77));
}

unsafe extern "C" fn stream_reply(u:*mut core::ffi::c_void,_:u32,pkt:u32,len:u32,fin:u32)->i32 {
    assert_eq!(fin,1);
    let f=f(u);
    let bytes=f.packets.remove(&pkt).expect("reply transfers exact owned packet");
    f.out.extend_from_slice(&bytes[..len as usize]);0
}
static STREAM:xip_abi::Stream=xip_abi::Stream{listen:None,version:1,size:core::mem::size_of::<xip_abi::Stream>() as u32,
    open:None,feed:None,close:None,write_pkt:Some(stream_reply),take:None,written:None};
unsafe extern "C" fn stream_components(u:*mut core::ffi::c_void,name:*const core::ffi::c_char)->*const core::ffi::c_void {
    if core::ffi::CStr::from_ptr(name).to_bytes()==b"stream" {(&STREAM as *const xip_abi::Stream).cast()}
    else {crate::psa_test::get_component(u,name)}
}
#[test]
fn stream_upload_matches_tcp_commit_and_rejects_corruption_without_socket_reads() {
    for corrupt in [false,true] {
        let image=module_image();
        let mut tcp=Fake::new(&image,target::MODULE,false);
        if corrupt {*tcp.input.last_mut().unwrap()^=1;}
        let (_,done)=run(&mut tcp);assert!(done);
        let mut f=Fake::new(&image,target::MODULE,false);
        f.input=tcp.input.clone();
        let mut h=f.host();h.get_component=Some(stream_components);
        h.socket_read=None;h.socket_write=None;
        let hw=unsafe{Hw::new(&h)}.unwrap();
        let mut c=Connection::new(hw,99,f.auth).unwrap();c.stream=true;
        let input=f.input.clone();let mut at=0;let mut done=false;
        for _ in 0..10000 {
            if let Some((id,rc))=f.job.take(){assert_eq!(id,c.job);c.waiting=false;if rc<0{c.fail(wire::FLASH);}else if c.work==Work::Erase{c.session.opened(true).unwrap();c.work=Work::None;}}
            if at<input.len(){
                match c.feed(hw,&input[at..(at+37).min(input.len())]){Ok(n)=>at+=n,Err(r)=>c.fail(r)}
            }else if !c.stream_fin {c.stream_fin=true;}
            if c.advance(hw){done=true;break;}
        }
        assert!(done,"stream upload must finish even while asynchronous flash work applies backpressure");
        assert_eq!(f.out,tcp.out);
        assert_eq!(f.flash,tcp.flash,"both transports have identical durable flash and journal state");
        assert_eq!(f.rescans,tcp.rescans);
        assert_eq!(c.rx,0,"QUIC body must never allocate a socket read packet");
        assert!(f.packets.is_empty(),"reply lease transfers once and is reclaimed");
    }
}
