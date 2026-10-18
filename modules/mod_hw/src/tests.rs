//! The module against a fake host table: alarms and interrupts are recorded and fired by hand.
use super::*;
use core::ffi::c_void;
use xip_abi::{CallView, Host};
use std::cell::RefCell;
use std::{vec, vec::Vec};

#[derive(Default)]
struct World {
    gpio: Vec<(i32, i32)>,
    events: Vec<(u16, u8, Vec<u8>)>,
    responses: Vec<Vec<u8>>,
    adc_mv: u32,
    now: u64,
    alarms: Vec<(i32, u32, u32, i32)>,   // id, delay, interval, method
    cancelled: Vec<i32>,
    registered: Vec<(i32, i32, i32, i32)>,
    unregistered: Vec<i32>,
    pktbuf: Vec<u8>,
}

fn w<'a>(user: *mut c_void) -> &'a RefCell<World> { unsafe { &*(user as *const RefCell<World>) } }

unsafe extern "C" fn log(_: *mut c_void, _: *const u8, _: usize) -> i32 { 0 }
unsafe extern "C" fn now_ms(user: *mut c_void) -> u64 { w(user).borrow().now }
// messages: a pool packet is a buffer of the world; `send` to the application with an id is a reply, without one an event
unsafe extern "C" fn pkt_alloc(user: *mut c_void, pkt: *mut u32) -> i32 { *pkt = 1; let _ = w(user); 0 }
unsafe extern "C" fn pkt_buf(user: *mut c_void, _pkt: u32, ptr: *mut *mut u8, max: *mut usize) -> i32 {
    let mut s = w(user).borrow_mut();
    if s.pktbuf.is_empty() { s.pktbuf = vec![0u8; 256]; }
    *ptr = s.pktbuf.as_mut_ptr(); *max = 256; 0
}
unsafe extern "C" fn pkt_free(_user: *mut c_void, _pkt: u32) -> i32 { 0 }
unsafe extern "C" fn send(user: *mut c_void, _pkt: u32, len: usize, _dst: u32, _c: i32, method: i32, id: u64) -> i32 {
    let mut s = w(user).borrow_mut();
    let data = s.pktbuf[..len].to_vec();
    if id != 0 { s.responses.push(data); } else { s.events.push((method as u16, 5, data)); }
    0
}
unsafe extern "C" fn get_setting(_: *mut c_void, _: *const u8, _: usize, _: *mut u8, _: usize, _: *mut usize) -> i32 { -7 }
unsafe extern "C" fn gpio_config(user: *mut c_void, pin: i32, _m: i32, _p: i32, level: i32) -> i32 {
    w(user).borrow_mut().gpio.push((pin, level)); 0
}
unsafe extern "C" fn gpio_write(user: *mut c_void, pin: i32, level: i32) -> i32 {
    w(user).borrow_mut().gpio.push((pin, level)); 0
}
unsafe extern "C" fn irq_register(user: *mut c_void, pin: i32, edge: i32, c: i32, m: i32) -> i32 {
    w(user).borrow_mut().registered.push((pin, edge, c, m)); 0
}
unsafe extern "C" fn irq_unregister(user: *mut c_void, pin: i32) -> i32 { w(user).borrow_mut().unregistered.push(pin); 0 }
unsafe extern "C" fn alarm_set(user: *mut c_void, delay: u32, _win: u32, interval: u32, _f: u32, _c: i32, m: i32) -> i32 {
    let mut s = w(user).borrow_mut();
    let id = s.alarms.len() as i32;
    s.alarms.push((id, delay, interval, m));
    id
}
unsafe extern "C" fn alarm_cancel(user: *mut c_void, id: i32) -> i32 { w(user).borrow_mut().cancelled.push(id); 0 }
unsafe extern "C" fn adc_read(user: *mut c_void, _pin: i32, _ref: u32, raw: *mut i32, mv: *mut u32, unit: *mut i32, ch: *mut i32) -> i32 {
    *raw = 2000; *mv = w(user).borrow().adc_mv; *unit = 1; *ch = 7; 0
}

/// The `hw` component of the fake: only what mod_hw calls.
static HARDWARE: xip_abi::Hardware = xip_abi::Hardware {
    version: xip_abi::HARDWARE_VERSION,
    size: core::mem::size_of::<xip_abi::Hardware>() as u32,
    gpio_config: Some(gpio_config),
    gpio_read: None,
    gpio_write: Some(gpio_write),
    irq_register: Some(irq_register),
    irq_unregister: Some(irq_unregister),
    irq_enable: None,
    spi_open: None,
    spi_transfer: None,
    spi_close: None,
    i2c_transfer: None,
    adc_read: Some(adc_read),
    rgbled_write: None,
};

/// The same without the ADC.
static HARDWARE_NO_ADC: xip_abi::Hardware = xip_abi::Hardware { adc_read: None, ..copy_hardware() };

const fn copy_hardware() -> xip_abi::Hardware {
    xip_abi::Hardware {
        version: HARDWARE.version, size: HARDWARE.size, gpio_config: HARDWARE.gpio_config, gpio_read: None, gpio_write: HARDWARE.gpio_write,
        irq_register: HARDWARE.irq_register, irq_unregister: HARDWARE.irq_unregister, irq_enable: None, spi_open: None, spi_transfer: None,
        spi_close: None, i2c_transfer: None, adc_read: HARDWARE.adc_read, rgbled_write: None,
    }
}

unsafe extern "C" fn get_component_no_adc(_user: *mut c_void, name: *const core::ffi::c_char) -> *const c_void {
    if core::ffi::CStr::from_ptr(name) == c"hw" { &HARDWARE_NO_ADC as *const xip_abi::Hardware as *const c_void } else { core::ptr::null() }
}

unsafe extern "C" fn get_component(_user: *mut c_void, name: *const core::ffi::c_char) -> *const c_void {
    if core::ffi::CStr::from_ptr(name) == c"hw" { &HARDWARE as *const xip_abi::Hardware as *const c_void } else { core::ptr::null() }
}

fn table(world: &RefCell<World>) -> Host {
    let mut h: Host = unsafe { core::mem::zeroed() };
    h.abi_version = xip_abi::ABI_VERSION;
    h.size = core::mem::size_of::<Host>() as u32;
    h.user = world as *const _ as *mut c_void;
    h.log = Some(log); h.now_ms = Some(now_ms); h.pkt_alloc = Some(pkt_alloc); h.pkt_buf = Some(pkt_buf); h.pkt_free = Some(pkt_free); h.send = Some(send);
    h.get_setting = Some(get_setting); h.get_component = Some(get_component); h.alarm_set = Some(alarm_set);
    h.alarm_cancel = Some(alarm_cancel);
    h
}

struct Rig {
    world: RefCell<World>,
    host: Host,
    module: HwModule,
}

impl Rig {
    fn new(adc_mv: u32) -> std::boxed::Box<Rig> {
        let world = RefCell::new(World { adc_mv, ..Default::default() });
        let host = table(&world);
        let mut rig = std::boxed::Box::new(Rig { world, host, module: HwModule::default() });
        rig.host.user = &rig.world as *const _ as *mut c_void;   // the box fixes the address
        rig
    }

    fn call(&mut self, method: i32, value: i32, data: &[u8], request: bool) -> i32 {
        let hw = unsafe { Hw::new(&self.host) }.unwrap();
        let view = CallView { component: COMPONENT, method, value, flags: request as u32, data, pkt: 0, from: 0, id: if request { 7 } else { 0 } };
        self.module.handle(&hw, &view)
    }
}

fn map(fields: &[(u64, i64)]) -> Vec<u8> {
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    w.map(fields.len());
    for (k, v) in fields { w.uint(*k).int(*v); }
    w.finish().unwrap().to_vec()
}

#[test]
fn battery_reports_millivolts_and_percent() {
    let mut rig = Rig::new(1900);
    // pin 35, ref 3300, divider 2.20, 3300..4200 mV
    let req = map(&[(1, 35), (2, 3300), (3, 220), (4, 3300), (5, 4200), (6, -1)]);
    assert_eq!(rig.call(M_BATTERY, 0, &req, true), 0);
    let w = rig.world.borrow();
    assert_eq!(w.events.len(), 1);
    let (id, kind, v) = &w.events[0];
    assert_eq!((*id, *kind), (EVENT_BATTERY, 5));
    // 1900 mV * 2.20 = 4180 mV => (4180-3300)*100/900 = 97 %
    let mut r = cbor::Reader::new(v);
    assert_eq!(r.next(), Some(Item::Array(8)));
    let vals: Vec<u64> = (0..8).map(|_| r.next().unwrap().as_uint().unwrap()).collect();
    assert_eq!((vals[3], vals[4]), (4180, 97));
    // the answer carries the same numbers
    let a = &w.responses[0];
    assert_eq!(cbor::get_int(a, 4), Ok(Some(4180)));
    assert_eq!(cbor::get_int(a, 5), Ok(Some(97)));
}

#[test]
fn a_battery_request_without_fields_uses_the_defaults() {
    let mut rig = Rig::new(1500);
    assert_eq!(rig.call(M_BATTERY, 0, &[], true), 0);
    assert_eq!(rig.world.borrow().events.len(), 1);
}

#[test]
fn the_control_pin_is_driven_then_the_sample_waits_for_an_alarm() {
    let mut rig = Rig::new(1500);
    let req = map(&[(1, 35), (3, 220), (6, 14), (7, 1)]);
    assert_eq!(rig.call(M_BATTERY, 0, &req, true), 0);
    {
        let w = rig.world.borrow();
        assert!(w.events.is_empty(), "nothing is measured before the pin settles");
        assert_eq!(w.alarms, [(0, SETTLE_MS, 0, M_BATTERY_SAMPLE)]);
        assert_eq!(w.gpio, [(14, 1), (14, 1)], "on");
    }
    rig.call(M_BATTERY_SAMPLE, 0, &[], false);
    let w = rig.world.borrow();
    assert_eq!(w.events.len(), 1);
    assert_eq!(w.gpio[2..], [(14, 0), (14, 0)], "released");
}

#[test]
fn a_bad_range_is_refused() {
    let mut rig = Rig::new(1500);
    assert_eq!(rig.call(M_BATTERY, 0, &map(&[(4, 4200), (5, 3300)]), true), 0);
    let w = rig.world.borrow();
    assert!(w.events.is_empty());
    assert_eq!(cbor::get_int(&w.responses[0], 9), Ok(Some(ERR_RANGE as i64)));
}

#[test]
fn a_probe_pin_field_that_is_not_a_list_is_refused() {
    let mut rig = Rig::new(1000);
    assert_eq!(rig.call(M_PROBE, 0, &map(&[(1, 3), (2, 500), (4, 0)]), true), 0);
    let w = rig.world.borrow();
    assert!(w.events.is_empty());
    assert_eq!(cbor::get_int(&w.responses[0], 9), Ok(Some(ERR_REQUEST as i64)));
}

#[test]
fn the_probe_with_a_pin_list() {
    let mut rig = Rig::new(1000);
    let mut buf = [0u8; 32];
    let mut wr = Writer::new(&mut buf);
    wr.map(3).uint(1).uint(3).uint(2).uint(500).uint(4).array(2).uint(34).uint(35);
    let req = wr.finish().unwrap().to_vec();
    assert_eq!(rig.call(M_PROBE, 0, &req, true), 0);
    assert_eq!(rig.world.borrow().events.len(), 2, "first round now");
    assert_eq!(rig.world.borrow().alarms, [(0, 500, 500, M_PROBE_TICK)]);
    rig.call(M_PROBE_TICK, 0, &[], false);
    rig.call(M_PROBE_TICK, 0, &[], false);
    let w = rig.world.borrow();
    assert_eq!(w.events.len(), 6, "three rounds of two pins");
    assert_eq!(w.cancelled, [0], "the alarm ends with the last sample");
}

#[test]
fn a_short_press_is_classified_on_release() {
    let mut rig = Rig::new(0);
    assert_eq!(rig.call(M_BUTTON, 0, &[], true), 0);
    assert_eq!(rig.world.borrow().registered, [(0, xip_abi::IRQ_BOTH as i32, COMPONENT, M_BUTTON_IRQ)]);
    rig.world.borrow_mut().now = 10_000;
    rig.call(M_BUTTON_IRQ, 0, &[], false);
    rig.world.borrow_mut().now = 10_120;
    rig.call(M_BUTTON_IRQ, 1, &[], false);
    let w = rig.world.borrow();
    assert_eq!(w.events.len(), 1);
    assert_eq!(w.events[0].0, EVENT_SHORT);
}

#[test]
fn long_and_double_presses() {
    let mut rig = Rig::new(0);
    rig.call(M_BUTTON, 0, &[], true);
    let mut press = |rig: &mut Rig, at: u64, held: u64| {
        rig.world.borrow_mut().now = at;
        rig.call(M_BUTTON_IRQ, 0, &[], false);
        rig.world.borrow_mut().now = at + held;
        rig.call(M_BUTTON_IRQ, 1, &[], false);
    };
    press(&mut rig, 10_000, 100);
    press(&mut rig, 10_300, 100);     // within 500 ms of the last release: double
    press(&mut rig, 20_000, 3_000);   // held: long
    let ids: Vec<u16> = rig.world.borrow().events.iter().map(|e| e.0).collect();
    assert_eq!(ids, [EVENT_SHORT, EVENT_DOUBLE, EVENT_LONG]);
}

#[test]
fn disabling_the_button_releases_the_interrupt() {
    let mut rig = Rig::new(0);
    rig.call(M_BUTTON, 0, &[], true);
    rig.call(M_BUTTON, 0, &map(&[(2, 0)]), true);
    assert_eq!(rig.world.borrow().unregistered, [0]);
}

#[test]
fn the_host_stop_releases_everything() {
    let mut rig = Rig::new(0);
    rig.call(M_BUTTON, 0, &[], true);
    let hw = unsafe { Hw::new(&rig.host) }.unwrap();
    let view = CallView { component: 0, method: xip_abi::HOST_STOP as i32, value: 0, flags: 0, data: &[], pkt: 0, from: 0, id: 0 };
    assert_eq!(rig.module.handle(&hw, &view), 0);
    assert_eq!(rig.world.borrow().unregistered, [0]);
}

#[test]
fn a_host_table_without_adc_answers_with_an_error() {
    let mut rig = Rig::new(0);
    rig.host.get_component = Some(get_component_no_adc);
    assert_eq!(rig.call(M_BATTERY, 0, &[], true), 0);
    let w = rig.world.borrow();
    assert_eq!(cbor::get_int(&w.responses[0], 9), Ok(Some(xip_abi::ERR_UNSUPPORTED as i64)));
}

#[test]
fn two_digit_dividers_are_parsed() {
    assert_eq!(parse_scaled(b"2.50", 0), 250);
    assert_eq!(parse_scaled(b"2.5", 0), 250);
    assert_eq!(parse_scaled(b"2.05", 0), 205);
    assert_eq!(parse_scaled(b"2.257", 0), 225);
    assert_eq!(parse_scaled(b"2", 0), 200);
    assert_eq!(parse_scaled(b"x", 7), 7);
}

/// The state block the image header asks for (Cargo.toml, `mkmod = "... --state N"`) is the one the code was built for.
#[test]
fn the_cargo_toml_asks_for_the_state_block_the_code_needs() {
    let toml = include_str!("../Cargo.toml");
    let want = std::format!("--state {}", crate::STATE_BYTES);
    assert!(toml.contains(&want), "Cargo.toml must say `{want}`");
}
