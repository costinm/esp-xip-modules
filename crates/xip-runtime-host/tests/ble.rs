//! The `ble` module (modules/mod_ble) over a fake `xip_ble` table: the table receives what the module decodes from the DMesh field tags
//! (`modules/mod_ble/API.md`), and what it fills in comes back as the DMesh `ble.status` tags. The real component is
//! components/xip_ble (ESP-IDF only); this checks the ABI layout, the tags and the error propagation on the host.
mod common;
use common::*;
use std::cell::RefCell;
use std::ffi::c_void;
use xip_abi::cbor::{self, Writer};
use xip_abi::*;
use xip_runtime_host::{Instance, Runtime};

fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 1100];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().expect("fits").len();
    buf.truncate(n);
    buf
}

#[derive(Default)]
struct Calls {
    start: Option<NimbleStart>,
    starts: u32,
    scan: Option<NimbleScan>,
    connect: Option<NimbleConnect>,
    mode: Option<(u8, u8)>,
    stops: u32,
    scan_stops: u32,
    clears: u32,
    suspends: u32,
    resumes: u32,
    refuse: i32,
}
thread_local! { static CALLS: RefCell<Calls> = RefCell::new(Calls::default()); }
fn rc() -> i32 { CALLS.with(|c| c.borrow().refuse) }

unsafe extern "C" fn f_start(_: *mut c_void, r: *const NimbleStart) -> i32 { CALLS.with(|c| { let mut c = c.borrow_mut(); c.start = Some(*r); c.starts += 1; }); rc() }
unsafe extern "C" fn f_stop(_: *mut c_void) -> i32 { CALLS.with(|c| c.borrow_mut().stops += 1); rc() }
unsafe extern "C" fn f_scan(_: *mut c_void, r: *const NimbleScan) -> i32 { CALLS.with(|c| c.borrow_mut().scan = Some(*r)); rc() }
unsafe extern "C" fn f_scan_stop(_: *mut c_void) -> i32 { CALLS.with(|c| c.borrow_mut().scan_stops += 1); rc() }
unsafe extern "C" fn f_connect(_: *mut c_void, r: *const NimbleConnect) -> i32 { CALLS.with(|c| c.borrow_mut().connect = Some(*r)); rc() }
unsafe extern "C" fn f_clear(_: *mut c_void) -> i32 { CALLS.with(|c| c.borrow_mut().clears += 1); rc() }
unsafe extern "C" fn f_mode(_: *mut c_void, r: *const BleMode) -> i32 { CALLS.with(|c| c.borrow_mut().mode = Some(((*r).mode, (*r).boot))); rc() }
unsafe extern "C" fn f_suspend(_: *mut c_void) -> i32 { CALLS.with(|c| c.borrow_mut().suspends += 1); rc() }
unsafe extern "C" fn f_resume(_: *mut c_void) -> i32 { CALLS.with(|c| c.borrow_mut().resumes += 1); rc() }
unsafe extern "C" fn f_status(_: *mut c_void, s: *mut BleStatus) -> i32 {
    let s = &mut *s;
    assert!(s.size as usize >= std::mem::size_of::<BleStatus>());
    s.ready = 1;
    s.advertising = 1;
    s.coc_connected = 1;
    s.generation = 7;
    s.handle = 0x40;
    s.coc_rx = 3;
    s.last_scan_rssi = -61;
    s.last_scan_addr = [1, 2, 3, 4, 5, 6];
    s.local_addr = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    s.local_addr_type = 1;
    s.egress_blocked_pending = 9;
    s.encrypted = 1;
    s.mode = BLE_MODE_RENDEZVOUS;
    s.suspended = 1;
    0
}
unsafe extern "C" fn f_results(_: *mut c_void, r: *mut BleScanResults) -> i32 {
    let r = &mut *r;
    r.results[..9].copy_from_slice(&[1, 2, 3, 4, 5, 6, 1, 0xc4, 1]);
    r.results_len = 9;
    0
}

static TABLE: Nimble = Nimble {
    version: NIMBLE_VERSION,
    size: std::mem::size_of::<Nimble>() as u32,
    start: Some(f_start), stop: Some(f_stop), scan: Some(f_scan), status: Some(f_status), scan_stop: Some(f_scan_stop),
    connect: Some(f_connect), bonds_clear: Some(f_clear), mode: Some(f_mode),
    scan_results: Some(f_results), suspend: Some(f_suspend), resume: Some(f_resume),
};

struct Console { _rt: Runtime, c: Instance }
impl Console {
    fn new() -> Self {
        CALLS.with(|c| *c.borrow_mut() = Calls::default());
        let rt = Runtime::default();
        rt.register_component("nimble", &TABLE as *const Nimble as *const c_void);
        let c = rt.add_module(module("mod_ble"), 32).unwrap();
        c.set_component(104);
        c.host_component("ble");
        c.start(b"");
        rt.run(0);
        Self { _rt: rt, c }
    }
    fn raw(&self, method: u64, fields: &[u8]) -> Vec<u8> {
        self.c.request(104, method as i32, fields).expect("module response")
    }
    fn ok(&self, method: u64, fields: &[u8]) -> Vec<u8> {
        let answer = self.raw(method, fields);
        assert!(cbor::get(&answer, 99).unwrap().is_none(), "error: {answer:02x?}");
        answer
    }
    fn error(&self, method: u64, fields: &[u8], error: i32) {
        let answer = self.raw(method, fields);
        assert_eq!(cbor::get_int(&answer, 99).unwrap(), Some(error as i64), "{answer:02x?}");
    }
}

const PAIRING: [u8; 16] = [0x80, 0x6f, 0x6b, 0x5f, 0x2a, 0x4f, 0x4a, 0x6f, 0x8c, 0x42, 0x4d, 0x65, 0x73, 0x68, 0x00, 0x01];
fn started() -> NimbleStart { CALLS.with(|c| c.borrow().start).expect("nimble.start was called") }
fn connected() -> NimbleConnect { CALLS.with(|c| c.borrow().connect).expect("nimble.connect was called") }
fn scanned() -> NimbleScan { CALLS.with(|c| c.borrow().scan).expect("nimble.scan was called") }

#[test]
fn dmesh_methods_80_to_85_keep_their_field_tags_and_the_module_owns_the_policy() {
    let k = Console::new();
    assert_eq!(k.ok(80, &[0xa0]), [0xa0]);
    // the module builds everything the component is told: the advertisement (flags, 0x1820, the 128-bit pairing UUID), the scan response
    // and the name, the intervals, the PSM and the CoC pool
    let s = started();
    assert_eq!((s.psm, s.coc_mtu, s.coc_buffers, s.adv_min, s.adv_max), (0x80, 1500, 3, 0x20, 0x40));
    assert_eq!(&s.adv[..s.adv_len as usize], [&[0x02, 0x01, 0x06, 0x03, 0x02, 0x20, 0x18, 0x11, 0x07][..], &PAIRING].concat());
    assert_eq!(&s.scan_rsp[..s.scan_rsp_len as usize], [6, 9, b'D', b'M', b'e', b's', b'h']);
    assert_eq!(&s.name[..s.name_len as usize], b"DMesh");
    k.ok(80, &enc(|w| { w.map(1).uint(1).boolean(true); }));
    let s = started();
    assert_eq!(s.adv[8 + 16], 0x02, "a paired device advertises the operational UUID");
    k.ok(81, &[0xa0]);
    k.ok(82, &enc(|w| { w.map(1).uint(2).uint(5000); }));
    let sc = scanned();
    assert_eq!(sc.duration_ms, 5000);
    // the scan filter: the 16-bit service UUID, and both 128-bit ones
    assert_eq!((sc.filters[0].ad_types, sc.filters[0].len, &sc.filters[0].data[..2]), ([2, 3], 2, &[0x20, 0x18][..]));
    assert_eq!((sc.filters[1].ad_types, sc.filters[1].len, sc.filters[1].data), ([6, 7], 16, PAIRING));
    assert_eq!(sc.filters[2].data[15], 2);
    k.ok(82, &[0xa0]);
    assert_eq!(scanned().duration_ms, 10_000, "absent: the default");
    k.ok(82, &enc(|w| { w.map(1).uint(2).uint(5); }));
    assert_eq!(scanned().duration_ms, 1_000, "clamped");
    k.ok(82, &enc(|w| { w.map(1).uint(2).uint(900_000); }));
    assert_eq!(scanned().duration_ms, 30_000);
    k.ok(84, &[0xa0]);
    k.ok(85, &enc(|w| { w.map(3).uint(2).bytes(&[6, 5, 4, 3, 2, 1]).uint(3).uint(1).uint(4).uint(0x81); }));
    let c = connected();
    assert_eq!((c.addr, c.addr_type, c.psm, c.coc_mtu), ([6, 5, 4, 3, 2, 1], 1, 0x81, 1500));
    k.ok(85, &enc(|w| { w.map(1).uint(2).bytes(&[6, 5, 4, 3, 2, 1]); }));
    assert_eq!(connected().psm, 0x80, "absent: the default PSM");
    // a PSM outside 0x0080..0x00ff is refused by the module before the component is called
    let before = CALLS.with(|c| c.borrow().connect.map(|c| c.psm));
    k.error(85, &enc(|w| { w.map(2).uint(2).bytes(&[6, 5, 4, 3, 2, 1]).uint(4).uint(0x100); }), ERR_ARG);
    k.error(85, &enc(|w| { w.map(1).uint(4).uint(0x81); }), ERR_ARG);
    assert_eq!(CALLS.with(|c| c.borrow().connect.map(|c| c.psm)), before);
    let c = CALLS.with(|c| (c.borrow().stops, c.borrow().scan_stops));
    assert_eq!(c, (1, 1));
}

#[test]
fn dmesh_clients_send_byte_fields_as_hex_text() {
    let k = Console::new();
    k.ok(85, &enc(|w| { w.map(3).uint(2).text("hex:060504030201").uint(3).uint(1).uint(4).uint(0x80); }));
    let c = connected();
    assert_eq!((c.addr, c.addr_type, c.psm), ([6, 5, 4, 3, 2, 1], 1, 0x80));
    k.error(85, &enc(|w| { w.map(1).uint(2).text("hex:0605"); }), ERR_ARG);
    k.error(85, &enc(|w| { w.map(1).uint(2).text("hex:zz00zz00zz00"); }), ERR_ARG);
    k.error(85, &enc(|w| { w.map(1).uint(2).text("hex:060504030"); }), ERR_ARG);
}

#[test]
fn status_carries_the_dmesh_tags_and_the_additions() {
    let k = Console::new();
    let s = k.ok(83, &[0xa0]);
    assert_eq!(cbor::get_int(&s, 5).unwrap(), Some(7));
    assert_eq!(cbor::get_int(&s, 6).unwrap(), Some(0x40));
    assert_eq!(cbor::get_int(&s, 7).unwrap(), Some(3));
    assert_eq!(cbor::get_int(&s, 14).unwrap(), Some(-61));
    assert_eq!(cbor::get_int(&s, 18).unwrap(), Some(1));
    assert_eq!(cbor::get_int(&s, 26).unwrap(), Some(9));
    assert_eq!(cbor::get_int(&s, 28).unwrap(), Some(BLE_MODE_RENDEZVOUS as i64));
    for tag in [1, 2, 4, 27, 30] {
        let mut r = cbor::get(&s, tag).unwrap().unwrap_or_else(|| panic!("tag {tag}"));
        assert!(matches!(r.next(), Some(cbor::Item::Bool(true))), "tag {tag}");
    }
    assert!(matches!(cbor::get(&s, 3).unwrap().and_then(|mut r| r.next()), Some(cbor::Item::Bool(false))));
    let mut addr = cbor::get(&s, 17).unwrap().unwrap();
    assert!(matches!(addr.next(), Some(cbor::Item::Bytes(b)) if b == [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    let mut addr = cbor::get(&s, 12).unwrap().unwrap();
    assert!(matches!(addr.next(), Some(cbor::Item::Bytes(b)) if b == [1, 2, 3, 4, 5, 6]));
}

#[test]
fn additions_listen_bonds_mode_results_and_sleep() {
    let k = Console::new();
    k.ok(87, &enc(|w| { w.map(1).uint(2).uint(0x90); }));
    let l = started();
    assert_eq!((l.psm, l.adv_len, l.coc_mtu, l.coc_buffers), (0x90, 0, 1500, 3), "listen: the server and the pool, no advertisement");
    k.ok(88, &[0xa0]);
    k.ok(89, &enc(|w| { w.map(2).uint(1).uint(BLE_MODE_RENDEZVOUS as u64).uint(2).uint(BLE_BOOT_ON as u64); }));
    assert_eq!(CALLS.with(|c| c.borrow().mode), Some((BLE_MODE_RENDEZVOUS, BLE_BOOT_ON)));
    let results = k.ok(90, &[0xa0]);
    let mut r = cbor::get(&results, 1).unwrap().unwrap();
    assert!(matches!(r.next(), Some(cbor::Item::Bytes(b)) if b == [1, 2, 3, 4, 5, 6, 1, 0xc4, 1]));
    k.ok(91, &[0xa0]);
    k.ok(92, &[0xa0]);
    let c = CALLS.with(|c| (c.borrow().clears, c.borrow().suspends, c.borrow().resumes));
    assert_eq!(c, (1, 1, 1));
}

#[test]
fn component_errors_and_bad_fields_come_back_as_error_codes() {
    let k = Console::new();
    CALLS.with(|c| c.borrow_mut().refuse = ERR_BUSY);
    k.error(82, &enc(|w| { w.map(1).uint(2).uint(1000); }), ERR_BUSY);
    CALLS.with(|c| c.borrow_mut().refuse = 0);
    // a field of the wrong size or type is refused before the component is called
    k.error(85, &enc(|w| { w.map(1).uint(2).bytes(&[1, 2, 3]); }), ERR_ARG);
    k.error(85, &enc(|w| { w.map(1).uint(4).text("psm"); }), ERR_ARG);
    assert!(CALLS.with(|c| c.borrow().connect.is_none()));
    // a host without the component
    let bare = Runtime::default();
    let c = bare.add_module(module("mod_ble"), 32).unwrap();
    c.set_component(104);
    c.start(b"");
    bare.run(0);
    let answer = c.request(104, 83, &[0xa0]).unwrap();
    assert_eq!(cbor::get_int(&answer, 99).unwrap(), Some(ERR_UNSUPPORTED as i64));
}

/// Requests arrive as 0x444d0002 datagrams: the runtime parses `{1: 104, 2: method, 3: id, 5: fields}` and wraps the answer; the module only sees requests.
fn over_datagrams() -> (Requests, Instance) {
    CALLS.with(|c| *c.borrow_mut() = Calls::default());
    let q = Requests::new();
    q.rt.register_component("nimble", &TABLE as *const Nimble as *const c_void);
    let c = q.rt.load_module(module("mod_ble")).unwrap();      // the hosted component name comes from the manifest (key 11); no port
    c.start(b"");
    q.rt.run(0);
    (q, c)
}
fn envelope(q: &Requests, method: u64, id: u64, fields: &[u8]) -> Vec<u8> {
    let mut request = enc(|w| { w.map(4).uint(1).uint(104).uint(2).uint(method).uint(3).uint(id).uint(5); });
    request.extend_from_slice(fields);
    let mut frames = q.send(&request);
    assert_eq!(frames.len(), 1, "one answer");
    frames.remove(0)
}

#[test]
fn envelopes_get_the_runtimes_answer_and_error_shape() {
    let (q, _c) = over_datagrams();
    let ok = envelope(&q, 83, 0x1_0000_0007, &[0xa0]);
    assert_eq!((cbor::get_int(&ok, 1).unwrap(), cbor::get_int(&ok, 2).unwrap(), cbor::get_int(&ok, 3).unwrap()), (Some(104), Some(83), Some(0x1_0000_0007)));
    assert!(cbor::get(&ok, 7).unwrap().is_none());
    let status = {
        let mut r = cbor::get(&ok, 6).unwrap().unwrap();
        let rest = r.rest();
        assert!(r.skip());
        rest[..rest.len() - r.rest().len()].to_vec()
    };
    assert_eq!(cbor::get_int(&status, 5).unwrap(), Some(7), "the same map as a direct request");
    // a field of the wrong type, a method this module does not have: `7: error`, never `{99: error}`
    let bad = envelope(&q, 85, 8, &enc(|w| { w.map(1).uint(2).bytes(&[1, 2, 3]); }));
    assert_eq!((cbor::get_int(&bad, 7).unwrap(), cbor::get_int(&bad, 3).unwrap()), (Some(ERR_ARG as i64), Some(8)));
    assert!(cbor::get(&bad, 6).unwrap().is_none() && cbor::get(&bad, 99).unwrap().is_none());
    let unknown = envelope(&q, 3, 9, &[0xa0]);
    assert_eq!(cbor::get_int(&unknown, 7).unwrap(), Some(ERR_UNSUPPORTED as i64));
    assert!(CALLS.with(|c| c.borrow().connect.is_none()));
    // the component's own refusal comes back the same way
    CALLS.with(|c| c.borrow_mut().refuse = ERR_BUSY);
    let busy = envelope(&q, 82, 11, &enc(|w| { w.map(1).uint(2).uint(1000); }));
    assert_eq!(cbor::get_int(&busy, 7).unwrap(), Some(ERR_BUSY as i64));
    // another component's envelope is not this module's: nobody serves it here, so nothing answers
    let mut other = enc(|w| { w.map(4).uint(1).uint(105).uint(2).uint(83).uint(3).uint(10).uint(5); });
    other.extend_from_slice(&[0xa0]);
    assert!(q.send(&other).is_empty());
}

unsafe extern "C" fn idle(_: *const Host, _: *mut u8, _: *const Call) -> i32 { 0 }

/// Another module reaches `ble` as a table: `get_component("ble")` is the runtime's copy of mod_ble's vtable, whose functions run in the module
/// (generated from API.md) and drive the `nimble` component.
#[test]
fn another_module_calls_the_ble_vtable_through_thunks() {
    let (rt, _ble) = {
        CALLS.with(|c| *c.borrow_mut() = Calls::default());
        let rt = Runtime::default();
        rt.register_component("nimble", &TABLE as *const Nimble as *const c_void);
        let m = rt.load_module(module("mod_ble")).unwrap();      // the component name comes from the module's manifest (key 11)
        m.start(b"");
        rt.run(0);
        (rt, m)
    };
    let caller = rt.add_handler("caller", idle, 16);
    caller.set_component(60);
    let h = caller.host();
    let table = unsafe { (h.get_component.unwrap())(h.user, c"ble".as_ptr()) } as *const Ble;
    assert!(!table.is_null());
    let t = unsafe { &*table };
    assert_eq!((t.version, t.size as usize), (1, std::mem::size_of::<Ble>()));
    let ctx = h.user; // the caller's own context: the thunk passes the module's instead
    let start = BleStart { paired: 1, ..Default::default() };
    assert_eq!(unsafe { t.start.unwrap()(ctx, &start) }, 0);
    let s = started();
    assert_eq!(s.adv[8 + 16], 0x02, "paired: the operational UUID, built by the module");
    let mut status = BleStatus::default();
    assert_eq!(unsafe { t.status.unwrap()(ctx, &mut status) }, 0);
    assert_eq!((status.generation, status.handle), (7, 0x40), "the component's answer came back through the module");
    let connect = BleConnect { addr: [1, 2, 3, 4, 5, 6], psm: 0x90, ..Default::default() };
    assert_eq!(unsafe { t.connect.unwrap()(ctx, &connect) }, 0);
    assert_eq!((connected().addr, connected().psm), ([1, 2, 3, 4, 5, 6], 0x90));
    let bad = BleConnect { addr: [1, 2, 3, 4, 5, 6], psm: 0x200, ..Default::default() };
    assert_eq!(unsafe { t.connect.unwrap()(ctx, &bad) }, ERR_ARG, "the module's checks apply to table callers too");
    CALLS.with(|c| c.borrow_mut().refuse = ERR_BUSY);
    let scan = BleScan { duration_ms: 2000, ..Default::default() };
    assert_eq!(unsafe { t.scan.unwrap()(ctx, &scan) }, ERR_BUSY);
    // the CBOR requests and the table are one implementation
    CALLS.with(|c| c.borrow_mut().refuse = 0);
    assert_eq!(unsafe { t.mode.unwrap()(ctx, &BleMode { mode: BLE_MODE_RENDEZVOUS, boot: 0, ..Default::default() }) }, 0);
    assert_eq!(CALLS.with(|c| c.borrow().mode), Some((BLE_MODE_RENDEZVOUS, 0)));
}
