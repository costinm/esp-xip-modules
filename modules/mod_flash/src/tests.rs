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
}
impl Fake {
    fn new(image: &[u8], target: u8, dry: bool) -> Self {
        Self::named(image, target, dry, b"test")
    }
    fn named(image: &[u8], target: u8, dry: bool, name: &[u8]) -> Self {
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        let pubkey: [u8; 65] = key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap();
        let mut state = StateLog::open(Raw(vec![255; 8192]));
        assert!(state.append(state::REC_PIN, &pubkey));
        let header = wire::Header::build(CPU, target, 2, image, name);
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
        h
    }
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
unsafe extern "C" fn stop(_: *mut core::ffi::c_void, _: *const u8, _: usize) -> i32 {
    0
}
unsafe extern "C" fn info(_: *mut core::ffi::c_void, out: *mut u8, n: usize) -> i32 {
    let b = core::slice::from_raw_parts_mut(out, n);
    let mut w = cbor::Writer::new(b);
    w.array(1).map(3).uint(1).text("test").uint(2).uint(2).uint(4).boolean(false);
    w.finish().unwrap().len() as i32
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
    assert_eq!(persisted.view.minver_module[0], Some(2));
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
    f.flash[at + 16..at + 18].copy_from_slice(&tag.to_le_bytes());
    f.flash[at + 44..at + 48].copy_from_slice(&64u32.to_le_bytes());
    let mut m = vec![0xa2, 1, 0x60 + name.len() as u8];
    m.extend_from_slice(name);
    m.extend_from_slice(&[2, tag as u8]);
    f.flash[at + 48..at + 52].copy_from_slice(&(m.len() as u32).to_le_bytes());
    f.flash[at + 64..at + 64 + m.len()].copy_from_slice(&m);
}
#[test]
fn a_module_goes_to_its_own_page_or_the_first_free_one() {
    let image = module_image(); // tag 2
    // pages 0 and 1 are other modules, "test" lives on page 3
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 0, b"a", 5);
    module_on_page(&mut f, 1, b"b", 6);
    module_on_page(&mut f, 3, b"test", 2);
    let (_, done) = run(&mut f);
    assert!(done, "{:?}", f.out);
    assert_eq!(&f.flash[0x380000 + 3 * 0x10000..][..image.len()], &image);
    let persisted = StateLog::open(Raw(f.flash[0xee000..0xf0000].to_vec()));
    assert_eq!(persisted.view.minver_module[3], Some(2));
    // a new module takes the first page that is not a module
    let mut f = Fake::new(&image, target::MODULE, false);
    module_on_page(&mut f, 0, b"a", 5);
    let (_, done) = run(&mut f);
    assert!(done);
    assert_eq!(&f.flash[0x380000 + 0x10000..][..image.len()], &image);
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
    assert!(!f.writes.iter().any(|(a, _)| *a == 0x390000));
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
