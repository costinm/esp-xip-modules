//! `mod_rtest` (the device test module), loaded as a shared object, against the host-runtime: virtual time, so alarms of a second cost
//! nothing. (These are the cases of the former C simulator; the C modules were dropped, see git history.)
mod common;
use common::*;
use std::ffi::c_void;
use xip_abi::*;
use xip_runtime_host::{Runtime, Instance};

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
    assert!(rt.pool_available() == avail - 1 && a.held_count() == 1);
    assert_eq!(a.send(2, 23, 0, &[]), 0);
    rt.run(1);
    assert!(rt.pool_available() == avail && a.held_count() == 0);

    // a module that ends with a kept packet gives it back, and it is counted
    assert_eq!(b.send(2, 22, 0, &msg), 0);
    rt.run(1);
    assert_eq!(rt.pool_available(), avail - 1);
    b.stop();
    rt.run(0);
    assert!(b.ended() && rt.pool_available() == avail && b.leaked() == 1);
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
