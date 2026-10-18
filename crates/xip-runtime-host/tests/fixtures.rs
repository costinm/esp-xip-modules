//! Fixtures and hooks of the host-runtime: SPI recording and replay (a `tcpdump` of the bus), the trace of host calls,
//! pins that have one owner.
mod common;
use common::*;
use std::cell::RefCell;
use std::rc::Rc;
use xip_abi::*;
use xip_runtime_host::fixtures::{self, Loopback, Recorder, Replay, SpiRecord};
use xip_runtime_host::{Runtime, TraceKind};

fn spi_module(rt: &Runtime) -> xip_runtime_host::Instance {
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    m.start(b"spi 1 2 3 4"); // sck miso mosi cs: the device is selected by its chip-select pin, 4
    m
}

#[test]
fn spi_transfers_can_be_recorded_saved_and_replayed() {
    // record what the module clocks through a (loopback) device
    let rt = Runtime::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    rt.set_spi_device(4, Box::new(Recorder { inner: Loopback, log: log.clone() }));
    let m = spi_module(&rt);
    rt.run(0);
    assert!(m.ended() && m.result() == RET_DONE as i32);
    let recording = log.borrow().clone();
    assert!(!recording.is_empty());

    // through a file and back
    let text = fixtures::save(&recording);
    assert_eq!(fixtures::load(&text).unwrap(), recording);

    // a fresh runtime with only the recording: the module behaves the same
    let rt = Runtime::default();
    let replay = Replay::new(fixtures::load(&text).unwrap(), false);
    let mismatch = replay.mismatch.clone();
    rt.set_spi_device(4, Box::new(replay));
    let m = spi_module(&rt);
    rt.run(0);
    assert!(m.ended() && m.result() == RET_DONE as i32, "{:?}", mismatch.borrow());
    assert!(mismatch.borrow().is_empty());

    // a recording that does not match what the module sends is a failure, and says where
    let mut wrong = recording.clone();
    wrong[0].tx[0] ^= 0xff;
    let rt = Runtime::default();
    let replay = Replay::new(wrong, false);
    let mismatch = replay.mismatch.clone();
    rt.set_spi_device(4, Box::new(replay));
    let m = spi_module(&rt);
    rt.run(0);
    assert!(m.ended() && m.result() < 0);
    assert!(mismatch.borrow()[0].starts_with("transfer 0"));
}

#[test]
fn a_recording_is_text_that_can_be_edited() {
    let r = fixtures::load("# a read of the status register\nt=5 tx=c0 rx=2a\n\nt=9 tx=1d00 rx=0000\n").unwrap();
    assert_eq!(r[0], SpiRecord { t_ms: 5, tx: vec![0xc0], rx: vec![0x2a] });
    assert_eq!(r.len(), 2);
    assert!(fixtures::load("t=1 tx=zz rx=00").is_err());
}

#[test]
fn the_trace_shows_handler_calls_and_host_calls() {
    let rt = Runtime::default();
    rt.set_trace(true);
    let m = spi_module(&rt);
    rt.run(0);
    assert!(m.ended());
    let t = rt.take_trace();
    assert!(t.iter().any(|e| e.kind == TraceKind::Handler { component: HOST_COMPONENT as i32, method: HOST_START as i32, result: RET_DONE as i32 }));
    let calls: Vec<_> = t.iter().filter_map(|e| if let TraceKind::Host(n) = e.kind { Some(n) } else { None }).collect();
    assert!(calls.contains(&"spi_open") && calls.contains(&"spi_transfer") && calls.contains(&"spi_close"));
}

unsafe extern "C" fn claim_pin_5(h: *const Host, _s: *mut u8, c: *const Call) -> i32 {
    let (h, c) = (&*h, &*c);
    if c.component == HOST_COMPONENT as i32 && c.method == HOST_START as i32 {
        let hw = (h.get_component.unwrap())(h.user, c"hw".as_ptr()) as *const Hardware;
        return ((*hw).gpio_config.unwrap())(h.user, 5, GPIO_OUTPUT as i32, GPIO_PULL_NONE as i32, 0);
    }
    0
}

#[test]
fn a_pin_has_one_owner() {
    let rt = Runtime::default();
    let a = rt.add_handler("a", claim_pin_5, 0);
    let b = rt.add_handler("b", claim_pin_5, 0);
    a.start(b"");
    b.start(b"");
    rt.run(0);
    assert_eq!(a.result(), 0);
    assert_eq!(b.result(), ERR_BUSY, "pin owned by another module");
}
