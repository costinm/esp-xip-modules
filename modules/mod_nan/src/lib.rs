//! `mod_nan`, first shape: a bench of the NAN discovery window against the public components only (`wifi`, `link`, `power`).
//! Start it with the console (`start hello`, its slot for now):
//!
//! disconnect (radio on, no station) -> sleep 500 ms -> promiscuous on, timer 20 ms -> count and print the frames -> promiscuous
//! off, sleep 500 ms (12 windows: a 500 ms sleep plus the resume drifts against the 512 TU window, so they sweep it) -> the wifi
//! intent as it was -> look at the station again after 8 s. The results go to the device log.
//!
//! The policy is the module's; the runtime owns the sequence (radio stop and start around the sleep, the early receive filter,
//! the UART). On a console on USB-Serial/JTAG the sleep is soft (`power.info soft_sleep`).
#![no_std]

use xip_abi::{
    CallView, Hw, Module, WifiIntent, WifiMonitor, WifiState, PowerInfo, PowerSleep, ALARM_EXACT, ALARM_WINDOW, CALL_LINK,
    CALL_LINK_EVENT, HOST_COMPONENT, HOST_START, HOST_WAKE, HOST_WIFI, RET_CONTINUE, RET_DONE,
};
const CONTINUE: i32 = RET_CONTINUE as i32;
const DONE: i32 = RET_DONE as i32;

/// The tag of the `hello` slot, this module's component for now.
const C_SELF: i32 = 46;
const M_FRAME: i32 = 1;
const M_WINDOW: i32 = 2;
const M_GUARD: i32 = 3;
const M_FINAL: i32 = 4;

const CHANNEL: u8 = 6;
const SLEEP_MS: u32 = 500;
const WINDOW_MS: u32 = 20;
const PASSES: u32 = 12;
const PRINT_FRAMES: u32 = 12;
const FINAL_MS: u32 = 8000;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Step {
    Idle,
    Disconnect,
    Sleep1,
    Window,
    Sleep2,
    Restore,
    Final,
    Done,
}

/// Bytes of state the image asks for (`mkmod` in Cargo.toml). An all-zero `State` is the initial state.
pub const STATE_BYTES: usize = 512;

pub struct State {
    step: Step,
    link: i32,
    frames: u32,
    total: u32,
    pass: u32,
    orig_generation: u32,
    orig: WifiIntent,
}

/// A line of text built in place.
struct Line {
    buf: [u8; 80],
    len: usize,
}

impl Line {
    fn new(s: &str) -> Line {
        let mut l = Line { buf: [0; 80], len: 0 };
        l.text(s);
        l
    }
    fn text(&mut self, s: &str) -> &mut Line {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        self
    }
    fn int(&mut self, v: i64) -> &mut Line {
        let mut digits = [0u8; 20];
        let mut n = 0;
        let mut u = v.unsigned_abs();
        loop {
            digits[n] = b'0' + (u % 10) as u8;
            n += 1;
            u /= 10;
            if u == 0 { break; }
        }
        if v < 0 { self.text("-"); }
        while n > 0 {
            n -= 1;
            let d = [digits[n]];
            self.text(core::str::from_utf8(&d).unwrap_or("?"));
        }
        self
    }
    fn hex(&mut self, bytes: &[u8]) -> &mut Line {
        const H: &[u8; 16] = b"0123456789abcdef";
        for b in bytes {
            let d = [H[(b >> 4) as usize], H[(b & 15) as usize]];
            self.text(core::str::from_utf8(&d).unwrap_or("?"));
        }
        self
    }
    fn log(&self, hw: &Hw) {
        hw.log(&self.buf[..self.len]);
    }
}

fn say(hw: &Hw, s: &str, v: i64) {
    Line::new(s).int(v).log(hw);
}

fn sleep_now(hw: &Hw) -> bool {
    // the marker DMesh sends on the UART / JTAG before a sleep (its `DMESH sleep enter`), so a person or a host watching the port can follow
    Line::new("DMESH sleep enter").log(hw);
    let Some(power) = hw.power() else { Line::new("nan: no power component").log(hw); return false };
    let Some(sleep) = power.sleep else { return false };
    let req = PowerSleep { duration_ms: SLEEP_MS, cycles: 1, awake_ms: 50, ..PowerSleep::default() };
    let rc = unsafe { sleep(hw.ctx(), &req) };
    if rc != 0 { say(hw, "nan: power.sleep failed ", rc as i64); }
    rc == 0
}

fn set_intent(hw: &Hw, want: &WifiIntent) -> bool {
    let Some(wifi) = hw.wifi() else { return false };
    let Some(set) = wifi.set else { return false };
    let mut generation = 0u32;
    let rc = unsafe { set(hw.ctx(), want, &mut generation) };
    if rc != 0 { say(hw, "nan: wifi.set failed ", rc as i64); }
    rc == 0
}

fn monitor(hw: &Hw, enable: bool) -> i32 {
    let Some(wifi) = hw.wifi() else { return -1 };
    let Some(f) = wifi.monitor else { return -1 };
    let m = WifiMonitor { enable: enable as u8, channel: if enable { CHANNEL } else { 0 }, ..WifiMonitor::default() };
    unsafe { f(hw.ctx(), &m) }
}

fn report(hw: &Hw, st: &mut State) {
    if let Some(info) = hw.power().and_then(|p| p.info.map(|f| (p, f))) {
        let mut i = PowerInfo::default();
        if unsafe { (info.1)(hw.ctx(), &mut i) } == 0 {
            say(hw, "nan: sleeps entered ", i.sleep_entries as i64);
            say(hw, "nan: sleeps rejected ", i.sleep_skipped as i64);
            say(hw, "nan: last sleep us ", i.last_duration_us as i64);
            say(hw, "nan: radio resume us ", i.last_resume_us as i64);
            say(hw, "nan: filter dropped ", i.filter_dropped as i64);
            say(hw, "nan: frames kept by the filter ", i.promisc_frames as i64);
            say(hw, "nan: soft sleep ", i.soft_sleep as i64);
        }
    }
    if let Some(state) = hw.wifi().and_then(|w| w.state) {
        let mut s = WifiState { size: core::mem::size_of::<WifiState>() as u32, ..WifiState::default() };
        if unsafe { state(hw.ctx(), &mut s) } == 0 {
            say(hw, "nan: wifi sta (0 off 1 joining 2 up) ", s.sta as i64);
            say(hw, "nan: wifi channel ", s.channel as i64);
            say(hw, "nan: wifi rssi ", s.rssi as i64);
        }
    }
    say(hw, "nan: frames in all windows ", st.total as i64);
    Line::new("nan: done").log(hw);
    st.step = Step::Done;
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        let host = call.component == HOST_COMPONENT as i32;
        if host && call.method == HOST_START as i32 {
            if hw.wifi().is_none() || hw.power().is_none() || hw.link().is_none() {
                Line::new("nan: needs wifi, power and link").log(hw);
                return DONE;
            }
            self.orig = WifiIntent::default();
            if let Some(get) = hw.wifi().and_then(|w| w.get) {
                unsafe { get(hw.ctx(), &mut self.orig, &mut self.orig_generation) };
            }
            if let Some(open) = hw.link().and_then(|l| l.open) {
                self.link = unsafe { open(hw.ctx(), c"promisc".as_ptr(), C_SELF, M_FRAME) };
            }
            say(hw, "nan: start; promisc link ", self.link as i64);
            // a window alarm makes the host deliver HOST_WAKE after each sleep; a guard ends the test if nothing happens
            hw.alarm_set(90_000, 0, 0, ALARM_WINDOW, C_SELF, M_GUARD);
            // disconnect: the radio stays on for ESP-NOW and promiscuous receive, the station leaves the access point
            let off = WifiIntent { radio: 1, sta: 0, ap: 0, channel: CHANNEL, ..WifiIntent::default() };
            self.step = Step::Disconnect;
            if !set_intent(hw, &off) {
                return DONE;
            }
            return CONTINUE;
        }
        if host && call.method == HOST_WIFI as i32 {
            if self.step == Step::Disconnect {
                Line::new("nan: disconnected, sleeping").log(hw);
                self.step = Step::Sleep1;
                if !sleep_now(hw) {
                    return DONE;
                }
            } else if self.step == Step::Restore {
                Line::new("nan: wifi intent applied; looking again later").log(hw);
                hw.alarm_set(FINAL_MS, 0, 0, ALARM_EXACT, C_SELF, M_FINAL);
                self.step = Step::Final;
            }
            return CONTINUE;
        }
        if host && call.method == HOST_WAKE as i32 {
            // the end-of-sleep marker, with why: 4 is the timer, anything else (UART, GPIO) is a person or a peer waking the device early
            Line::new("DMESH sleep exit cause ").int(call.value as i64).log(hw);
            if call.value != 4 && (self.step == Step::Sleep1 || self.step == Step::Sleep2) {
                // a packet while sleeping swaps the device to the active state: no more windows, the wifi as it was
                Line::new("nan: woken early, going active").log(hw);
                self.step = Step::Restore;
                let orig = self.orig;
                if !set_intent(hw, &orig) || orig.radio == 0 {
                    report(hw, self);
                    return DONE;
                }
                return CONTINUE;
            }
            if self.step == Step::Sleep1 {
                let rc = monitor(hw, true);
                say(hw, "nan: awake, promiscuous on, rc ", rc as i64);
                self.step = Step::Window;
                self.frames = 0;
                hw.alarm_set(WINDOW_MS, 0, 0, ALARM_EXACT, C_SELF, M_WINDOW);
            } else if self.step == Step::Sleep2 {
                Line::new("nan: awake, wifi back").log(hw);
                self.step = Step::Restore;
                let orig = self.orig;
                if !set_intent(hw, &orig) || orig.radio == 0 {
                    report(hw, self);
                    return DONE;
                }
            }
            return CONTINUE;
        }
        if call.component == C_SELF && call.method == M_WINDOW && self.step == Step::Window {
            monitor(hw, false);
            say(hw, "nan: window over, frames ", self.frames as i64);
            self.total += self.frames;
            self.pass += 1;
            self.step = if self.pass < PASSES { Step::Sleep1 } else { Step::Sleep2 };
            if !sleep_now(hw) {
                self.step = Step::Done;
                return DONE;
            }
            return CONTINUE;
        }
        if call.component == C_SELF && (call.method == M_FINAL || call.method == M_GUARD) {
            if call.method == M_GUARD {
                Line::new("nan: timeout").log(hw);
            }
            report(hw, self);
            return DONE;
        }
        if call.flags & CALL_LINK != 0 && call.flags & CALL_LINK_EVENT == 0 {
            self.frames += 1;
            if self.frames <= PRINT_FRAMES && call.data.len() >= 24 {
                let f = call.data;
                Line::new("nan: frame type ").hex(&f[..1]).text(" a3 ").hex(&f[16..22]).text(" r ")
                    .int(call.value as i16 as i64).text(" l ").int(f.len() as i64).log(hw);
            }
        }
        CONTINUE
    }
}

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}
