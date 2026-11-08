//! `mod_rtest` (the device test module), loaded as a shared object, against the host-runtime: virtual time, so alarms of a second cost
//! nothing. (These are the cases of the former C simulator; the C modules were dropped, see git history.)
mod common;
use common::*;
use std::ffi::c_void;
use xip_abi::*;
use xip_runtime_host::{Runtime, Instance, Loaded};

fn selftest(name: &str) {
    let (rt, m) = started(name, "selftest");
    rt.run(0);
    if m.result() < 0 {
        dump(&m);
    }
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert!(m.find_event(0x7eff, "fail=0").is_some());
    for id in 0x7e00..0x7e50 {
        assert!(m.find_event(id, "FAIL").is_none(), "check 0x{id:04x} failed");
    }
    assert!(m.events().len() > 35);
    assert_eq!(m.active_alarms(), 0);
}

fn serve_request_stop(name: &str) {
    let (rt, m) = started(name, "serve");
    rt.run(10);
    assert!(!m.ended());
    let one = [0xa1, 0x01, 0x63, b'o', b'n', b'e'];
    let two = [0xa1, 0x01, 0x63, b't', b'w', b'o'];
    assert_eq!(m.post(2, 1, &one), 0);
    rt.run(1);
    assert!(m.find_event(0x7e10, "one").is_some());
    assert_eq!(m.request(2, 1, &two).unwrap(), two);
    let info = m.request(2, 2, &[]).unwrap(); // {1: calls, 2: mode}
    assert!(info.len() > 3 && info[0] == 0xa2);
    let calls = m.calls();
    rt.run(3600 * 1000); // an hour of idle: the module is not called
    assert_eq!(m.calls(), calls);
    m.stop();
    rt.run(0);
    assert!(m.ended() && m.result() == 0);
    assert!(m.find_event(0x7eff, "stopped").is_some());
}

/// Packets between the application and a module, between two modules, kept past a call, and given back when the module ends.
fn packets(name: &str) {
    let rt = Runtime::default();
    let a = rt.add_module(module(name), 64).unwrap();
    let b = rt.add_module(module(name), 64).unwrap();
    a.start(b"serve");
    b.start(b"serve");
    a.set_component(2);
    b.set_component(3);
    rt.run(1);
    let avail = rt.pool_available();

    // application -> module A -> back to the application: one packet, the same one, never copied
    let msg = [0x10, 0x20, 0x30];
    assert_eq!(a.send(2, 20, 77, &msg), 0);
    rt.run(1);
    {
        let s = rt.state();
        assert_eq!(s.main.len(), 1);
        let m = &s.main[0];
        assert!(m.id == 77 && m.data.len() == 3 && m.from == 2 && m.component == 2 && m.method == 21);
        assert!(m.data[0] == !0x10u8 && m.data[2] == 0x30, "first byte flipped in place");
    }
    assert_eq!(rt.pool_available(), avail, "the packet went back to the pool");

    // a second instance answers with its own address
    assert_eq!(b.send(2, 20, 5, &msg), 0);
    rt.run(1);
    {
        let s = rt.state();
        assert!(s.main.len() == 2 && s.main[1].from == 3);
    }
    assert_eq!(rt.pool_available(), avail);

    // keep: the packet stays out of the pool until it is dropped
    assert_eq!(a.send(2, 22, 0, &msg), 0);
    rt.run(1);
    assert!(rt.pool_available() == avail - 1);
    assert_eq!(a.send(2, 23, 0, &[]), 0);
    rt.run(1);
    assert!(rt.pool_available() == avail);

    // a module that ends with a kept packet gives it back, and it is counted
    assert_eq!(b.send(2, 22, 0, &msg), 0);
    rt.run(1);
    assert_eq!(rt.pool_available(), avail - 1);
    b.stop();
    rt.run(0);
    assert!(b.ended() && rt.pool_available() == avail && b.leaked() == 0);
    assert_ne!(b.send(2, 20, 0, &msg), 0, "nobody home");
    assert_eq!(rt.pool_available(), avail);

    // the runtime-side queue is bounded: a sender backs off when it is full, and succeeds again as the module takes packets
    let (mut sent, mut rc) = (0, 0);
    while sent < 10 {
        rc = a.send(2, 23, 0, &[]);
        if rc != 0 {
            break;
        }
        sent += 1;
    }
    assert!(sent == 4 && rc == ERR_BUSY);
    rt.run(1);
    assert_eq!(a.send(2, 23, 0, &[]), 0);
    rt.run(1);
    assert_eq!(rt.pool_available(), avail);

    // too large for a packet
    let big = vec![0u8; PKT_MAX as usize + 1];
    assert_eq!(a.send(2, 20, 0, &big), ERR_SIZE);
    drop((a, b));
}

/// The managed heap: what a module did not free is freed by the host when it ends, and counted.
fn memory(name: &str) {
    let (rt, s) = started(name, "serve");
    s.set_component(2);
    rt.run(1);
    assert_eq!(s.send(2, 24, 0, &[]), 0);
    rt.run(1);
    assert_eq!(s.mem_leaked(), 0, "still running: nothing counted");
    s.stop();
    rt.run(0);
    assert!(s.ended() && s.mem_leaked() == 64, "the balance: the 64 bytes it took and did not give back");
}

fn alarms(name: &str) {
    let (rt, m) = started(name, "alarm");
    rt.run(100);
    if !m.ended() || m.result() != RET_DONE as i32 {
        dump(&m);
    }
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert!(m.find_event(0x7eff, "fail=0").is_some());
}

fn gpio_loop(name: &str) {
    let (rt, m) = started(name, "gpio 4 5");
    rt.wire(4, 5);
    rt.run(200);
    if m.result() < 0 || !m.ended() {
        dump(&m);
    }
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert!(m.find_event(0x7e00 + 5, "irq-event ok").is_some());
    // no wire: the checks must notice
    let (rt, m) = started(name, "gpio 4 5");
    rt.run(200);
    assert!(m.ended() && m.result() < 0);
}

fn spi_loop(name: &str) {
    let (rt, m) = started(name, "spi 1 2 3 4");
    rt.run(0);
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert!(m.spi_transfers() >= 1);
}

macro_rules! suite {
    ($module:literal, $($case:ident),*) => {
        $( #[test] fn $case() { super::$case($module) } )*
    };
}
mod mod_rtest {
    suite!("mod_rtest", selftest, serve_request_stop, packets, memory, alarms, gpio_loop, spi_loop);
}

// ---- alarm rules of the host, driven through the host table (a module that only schedules) ----

#[repr(C)]
struct Probe {
    fired: i32,
    last_late: i32,
    at: [u64; 8],
}

unsafe extern "C" fn probe_handle(h: *const Host, state: *mut u8, c: *const Call) -> i32 {
    let (h, p, c) = (&*h, &mut *(state as *mut Probe), &*c);
    if c.component == HOST_COMPONENT as i32 {
        if c.method == HOST_START as i32 {
            // the first byte of the request is the scenario: 1 flexible, 2 connected, 3 periodic, 4 flexible beyond the window
            let scenario = if c.len > 0 { *c.data - b'0' } else { 0 };
            let alarm = h.alarm_set.unwrap();
            match scenario {
                1 => { alarm(h.user, 1000, 5000, 0, ALARM_FLEXIBLE, 9, 1); }
                2 => { alarm(h.user, 100, 0, 0, ALARM_CONNECTED, 9, 2); }
                3 => { alarm(h.user, 100, 0, 100, ALARM_EXACT, 9, 3); }
                4 => { alarm(h.user, 1000, 1000, 0, ALARM_FLEXIBLE, 9, 1); }
                _ => {}
            }
        }
        return 0;
    }
    if p.fired < 8 {
        p.at[p.fired as usize] = (h.now_ms.unwrap())(h.user);
    }
    p.fired += 1;
    p.last_late = c.value;
    0
}

fn probe(request: &str) -> (Runtime, Instance) {
    let rt = Runtime::default();
    let m = rt.add_handler("probe", probe_handle, std::mem::size_of::<Probe>());
    m.start(request.as_bytes());
    (rt, m)
}

#[test]
fn alarm_flexible_fires_in_the_wake_window() {
    let (rt, m) = probe("1");
    rt.set_window(3000, 10000);
    rt.run(0);
    rt.run(2999);
    assert_eq!(m.calls(), 1, "only START so far");
    rt.run(2);
    assert_eq!(m.calls(), 2);
}

#[test]
fn alarm_flexible_beyond_the_window_fires_as_late_as_allowed() {
    let (rt, m) = probe("4");
    rt.set_window(60000, 60000);
    rt.run(1999);
    assert_eq!(m.calls(), 1);
    rt.run(2);
    assert_eq!(m.calls(), 2);
}

#[test]
fn alarm_without_a_window_is_exact() {
    let (rt, m) = probe("1");
    rt.run(999);
    assert_eq!(m.calls(), 1);
    rt.run(2);
    assert_eq!(m.calls(), 2);
}

#[test]
fn alarm_connected_waits_for_connectivity() {
    let (rt, m) = probe("2");
    rt.run(5000);
    assert!(m.calls() == 1 && m.active_alarms() == 1);
    rt.set_connected(true);
    rt.run(1);
    assert_eq!(m.calls(), 3, "START, NETWORK, the alarm");
    assert_eq!(m.active_alarms(), 0);
}

#[test]
fn alarm_periodic_is_rearmed_from_the_schedule() {
    let (rt, m) = probe("3");
    rt.run(1000);
    assert_eq!(m.calls(), 11, "ten calls in a second plus START");
}

#[allow(dead_code)]
fn _keep(_: *mut c_void) {}

#[test]
fn now_bench_rejects_truncated_or_corrupt_payloads_and_generates_full_response() {
    use xip_abi::cbor;
    let rt = Runtime::default();
    rt.add_native_link("espnow", PKT_MAX, xip_runtime_host::NativeTx::Done);
    rt.set_quic_classifier(true);
    let m = rt.add_module(module("mod_rtest"), 512).unwrap();
    m.set_component(2);
    m.start(b"now listen size=1448 secs=60");
    rt.run(0);
    // The bench speaks CBOR datagrams: long header (0xc0, 0x444d0002, DCID 01, SCID 02), envelope {1: 2, 2: 40 ping, 3: seq, 4: size, 5: pattern}.
    let byte = |seq: u32, i: usize| (seq.rotate_left((i % 32) as u32) ^ (i as u32).wrapping_mul(73) ^ ((i as u32) >> 8)) as u8;
    let mut payload = vec![0xc0, 0x44, 0x4d, 0x00, 0x02, 1, 0x01, 1, 0x02];
    payload.extend([0xa5, 0x01, 0x02, 0x02, 0x18, 40, 0x03, 0x1a, 0, 0, 0, 37, 0x04, 0x1a, 0, 0, 0x05, 0xa8, 0x05, 0x59]);
    payload.extend((1448u16 - 31).to_be_bytes());
    payload.extend((0..1448 - 31).map(|i| byte(37, i)));
    assert_eq!(payload.len(), 1448);
    assert_eq!(rt.inject_frame("espnow", 0x060504030201, 0, &payload), 0);
    rt.run(0);
    let sent = rt.take_link_tx("espnow");
    assert_eq!(sent.len(), 1);
    // the answer: the runtime added the long header (CIDs swapped); the envelope is a pong with the same pattern
    assert_eq!(sent[0].1.len(), 1448);
    assert_eq!(&sent[0].1[..9], &[0xc0, 0x44, 0x4d, 0x00, 0x02, 1, 0x02, 1, 0x01]);
    assert_eq!(cbor::get_int(&sent[0].1[9..], 2).unwrap(), Some(41));
    assert_eq!(&sent[0].1[9 + 22..], &payload[9 + 22..]);
    // Header still declares 1448: truncation cannot be accepted as a shorter packet.
    assert_eq!(rt.inject_frame("espnow", 0x060504030201, 0, &payload[..1447]), 0);
    rt.run(0);
    payload[1447] ^= 1;
    assert_eq!(rt.inject_frame("espnow", 0x060504030201, 0, &payload), 0);
    rt.run(0);
    assert!(rt.take_link_tx("espnow").is_empty());
    let info = m.request(2, 2, &[0xa0]).unwrap();
    assert_eq!(rt.quic_stats().long_cbor_bad, 1, "the truncated envelope never reaches the module");
    assert_eq!(cbor::get_int(&info, 7).unwrap(), Some(1));
    assert_eq!(cbor::get_int(&info, 10).unwrap(), Some(1));
    assert_eq!(cbor::get_int(&info, 11).unwrap(), Some(1448));
    m.stop(); rt.run(0);
    assert_eq!(rt.wifi_intent().0.radio, 0, "restore radio intent on cancellation");
    let rt = Runtime::default();
    rt.add_native_link("espnow", PKT_MAX, xip_runtime_host::NativeTx::Done);
    let m = rt.add_module(module("mod_rtest"), 512).unwrap(); m.set_component(2);
    rt.set_quic_classifier(true);
    m.start(b"now listen rxonly size=1448 secs=60"); rt.run(0);
    payload[1447] ^= 1;
    assert_eq!(rt.inject_frame("espnow", 0x060504030201, 0, &payload), 0); rt.run(0);
    assert!(rt.take_link_tx("espnow").is_empty(), "RX-only must not generate an echo");
    let info = m.request(2, 2, &[0xa0]).unwrap();
    assert_eq!(cbor::get_int(&info, 7).unwrap(), Some(1));
    assert_eq!(cbor::get_int(&info, 9).unwrap(), Some(0));
    m.stop(); rt.run(0);
    let bad = rt.add_module(module("mod_rtest"), 512).unwrap();
    bad.start(format!("now size={}", PKT_MAX + 1).as_bytes()); rt.run(0);
    assert!(bad.ended() && bad.result() == ERR_SIZE, "normal ABI cap is independent of radio capability");
}

// ---- state objects (`Host::state_resize` / `state_get`): a handler that runs a script of state calls at its start ----

thread_local! {
    static STATE_RESULTS: std::cell::RefCell<Vec<(i32, i64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Script ops, 13 bytes each: code (u8), key, a, b (u32 LE). The handler appends one `(rc, value)` per op:
/// 0 resize(key, class a, size b); 1 get(key) -> size; 2 fill(key, byte a); 3 prefix(key, byte a, n b) -> 1 if bytes [0, n) are a;
/// 5 zero(key, from a) -> 1 if bytes [a, size) are zero.
unsafe extern "C" fn state_handle(h: *const Host, _state: *mut u8, c: *const Call) -> i32 {
    let c = &*c;
    if c.component != HOST_COMPONENT as i32 || c.method != HOST_START as i32 || c.len == 0 {
        return RET_DONE as i32;
    }
    let hw = Hw::new(h).expect("the host table");
    let script = std::slice::from_raw_parts(c.data, c.len);
    STATE_RESULTS.with(|r| r.borrow_mut().clear());
    for op in script.chunks_exact(13) {
        let u = |i: usize| u32::from_le_bytes(op[i..i + 4].try_into().unwrap());
        let (code, key, a, b) = (op[0], u(1), u(5), u(9));
        let res = match code {
            0 => match hw.state_resize(key, a, b as usize) {
                Ok(_) => (0, 0),
                Err(e) => (e, 0),
            },
            1 => match hw.state_get(key) {
                Ok((_, n)) => (0, n as i64),
                Err(e) => (e, 0),
            },
            2 => match hw.state_get(key) {
                Ok((p, n)) => {
                    if n > 0 {
                        std::ptr::write_bytes(p, a as u8, n);
                    }
                    (0, 0)
                }
                Err(e) => (e, 0),
            },
            3 => match hw.state_get(key) {
                Ok((p, n)) => {
                    let ok = b as usize <= n && (0..b as usize).all(|i| *p.add(i) == a as u8);
                    (0, ok as i64)
                }
                Err(e) => (e, 0),
            },
            5 => match hw.state_get(key) {
                Ok((p, n)) => (0, ((a as usize)..n).all(|i| *p.add(i) == 0) as i64),
                Err(e) => (e, 0),
            },
            _ => (ERR_ARG, 0),
        };
        STATE_RESULTS.with(|r| r.borrow_mut().push(res));
    }
    RET_DONE as i32
}

fn script(list: &[(u8, u32, u32, u32)]) -> Vec<u8> {
    list.iter()
        .flat_map(|&(code, key, a, b)| {
            let mut v = vec![code];
            v.extend(key.to_le_bytes());
            v.extend(a.to_le_bytes());
            v.extend(b.to_le_bytes());
            v
        })
        .collect()
}

/// Start an instance of `name` at `version` that runs `ops`; it ends when the script is done. Returns the instance and the results.
fn run_state(rt: &Runtime, name: &str, version: u32, ops: &[(u8, u32, u32, u32)]) -> (Instance, Vec<(i32, i64)>) {
    let m = rt.add_loaded(name, Loaded::linked_with_version(state_handle, version), 0);
    m.start(&script(ops));
    rt.run(0);
    assert!(m.ended(), "the script ends the instance");
    (m, STATE_RESULTS.with(|r| r.borrow().clone()))
}

const INST: u32 = STATE_INSTANCE;
const RET: u32 = STATE_RETAINED;

#[test]
fn instance_state_grows_shrinks_and_frees_keeping_contents() {
    let rt = Runtime::default();
    let (m, res) = run_state(&rt, "grow", 1, &[
        (0, 1, INST, 16), (2, 1, 0x7e, 0), (0, 1, INST, 40), (3, 1, 0x7e, 16), (5, 1, 16, 0),
        (0, 1, INST, 8), (1, 1, 0, 0), (0, 1, INST, 0), (1, 1, 0, 0),
    ]);
    assert_eq!(res, vec![(0, 0), (0, 0), (0, 0), (0, 1), (0, 1), (0, 0), (0, 8), (0, 0), (ERR_NOTFOUND, 0)]);
    assert_eq!(m.mem_leaked(), 0, "instance objects are freed when the instance ends");
}

#[test]
fn state_classes_must_match_and_reserved_classes_are_unsupported() {
    let rt = Runtime::default();
    let (_, res) = run_state(&rt, "classes", 1, &[
        (0, 1, INST, 8), (0, 1, RET, 8), (0, 2, RET, 8), (0, 2, INST, 8),
        (0, 3, STATE_PSRAM, 8), (0, 4, STATE_RTC, 8), (0, 5, 9, 8), (0, 0, RET, 8), (1, 0, 0, 0),
    ]);
    assert_eq!(res, vec![
        (0, 0), (ERR_ARG, 0), (0, 0), (ERR_ARG, 0), (ERR_UNSUPPORTED, 0), (ERR_UNSUPPORTED, 0), (ERR_ARG, 0), (ERR_ARG, 0), (ERR_NOTFOUND, 0),
    ], "a retained key is not an instance key, reserved classes are unsupported, a class beyond 3 is an argument error");
}

#[test]
fn instance_state_is_held_to_the_managed_heap_quota() {
    // the runtime's default quota is 8192 bytes
    let rt = Runtime::default();
    let (m, res) = run_state(&rt, "quota", 1, &[
        (0, 1, INST, 4000), (0, 2, INST, 4000), (0, 3, INST, 1000), (0, 1, INST, 4100), (0, 1, INST, 5000), (1, 1, 0, 0),
    ]);
    assert_eq!(res, vec![(0, 0), (0, 0), (ERR_SIZE, 0), (0, 0), (ERR_SIZE, 0), (0, 4100)], "a grow counts the new size, not the old one");
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn retained_state_survives_the_instance_and_its_restart_of_the_same_version() {
    let rt = Runtime::default();
    let (first, _) = run_state(&rt, "keeper", 1, &[(0, 7, RET, 64), (2, 7, 0x42, 0), (0, 8, INST, 100)]);
    assert_eq!(first.mem_leaked(), 0, "retained memory is not a leak; the instance object is freed");
    let (second, res) = run_state(&rt, "keeper", 1, &[(1, 7, 0, 0), (3, 7, 0x42, 64), (1, 8, 0, 0)]);
    assert_eq!(res, vec![(0, 64), (0, 1), (ERR_NOTFOUND, 0)], "the retained object is there, the instance one is not");
    assert_eq!(second.mem_leaked(), 0);
}

#[test]
fn retained_state_is_dropped_when_the_module_starts_with_another_version() {
    let rt = Runtime::default();
    run_state(&rt, "upgraded", 1, &[(0, 7, RET, 32), (2, 7, 0x11, 0)]);
    let (_, res) = run_state(&rt, "upgraded", 2, &[(1, 7, 0, 0), (0, 7, RET, 32), (3, 7, 0, 32)]);
    assert_eq!(res, vec![(ERR_NOTFOUND, 0), (0, 0), (0, 1)], "version 2 starts with nothing of version 1; its own object is zeroed");
    let (_, res) = run_state(&rt, "upgraded", 1, &[(1, 7, 0, 0)]);
    assert_eq!(res, vec![(ERR_NOTFOUND, 0)], "version 1 does not get back what version 2 replaced");
}

#[test]
fn retained_caps_are_per_name_objects_and_device_bytes() {
    let rt = Runtime::default();
    let mut ops: Vec<(u8, u32, u32, u32)> = (1..=8).map(|k| (0, k, RET, 8)).collect();
    ops.push((0, 9, RET, 8));
    ops.push((0, 10, INST, 8));
    let (_, res) = run_state(&rt, "few", 1, &ops);
    assert!(res[..8].iter().all(|&r| r == (0, 0)), "eight retained objects per name");
    assert_eq!(res[8], (ERR_SIZE, 0), "the ninth of the name is over its cap");
    assert_eq!(res[9], (0, 0), "an instance object is not a retained one: it has its own quota");
    let rt = Runtime::default();   // a runtime of its own: the cap is device-wide
    let (_, res) = run_state(&rt, "bulk", 1, &[(0, 11, RET, 8000), (0, 12, RET, 300), (0, 13, RET, 192)]);
    assert_eq!(res, vec![(0, 0), (ERR_SIZE, 0), (0, 0)], "the device cap is 8192 bytes of retained objects in all");
}

#[test]
fn the_state_root_is_resizable_and_not_charged_to_the_quota() {
    // the module's own state (key 0) is not counted in its heap quota, as the image's state block never was
    let rt = Runtime::default();
    let (m, res) = run_state(&rt, "root", 1, &[(0, 0, INST, 4000), (0, 0, INST, 4100), (0, 1, INST, 8000), (1, 0, 0, 0)]);
    assert_eq!(res, vec![(0, 0), (0, 0), (0, 0), (0, 4100)]);
    assert_eq!(m.mem_leaked(), 0);
}
