//! `mod_rtest`: the device test of the ABI in Rust, the device test module (component 2, the `test` slot). The tests (`tests/device_test.py`, the console) drive it. It calls every entry of the host table and reports one event per check; on top of the C module it uses `alloc` through
//! both allocators of `xip-abi` (the global one and an explicit one) and checks that a failed allocation is an error, not an abort.
//!
//! The start request picks the mode: empty or `selftest` (every check that needs no wiring; finishes with 0 when all pass),
//! `serve`, `alarm`, `gpio A B`, `spi SCK MISO MOSI CS [BUS]`. Events: `0x7e00 + n` per check, text `"<name> ok"` or
//! `"<name> FAIL <code>"`; `0x7eff` the summary.
//!
//! Like every module it never waits and has no writable statics: what it remembers is the state block, and every `Vec` is
//! freed before the handler returns (a block still held when the module stops is `ERR_LEAK`).
#![no_std]
#![feature(allocator_api)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use xip_abi::cbor::{self, Writer};
use xip_abi::{
    CallView, Hw, Module, SpiConfig, ADDR_MAIN, ALARM_EXACT, ERR_AGAIN, ERR_ARG, ERR_BUSY, ERR_NOMEM, ERR_NOTFOUND, ERR_SIZE,
    GPIO_INPUT, GPIO_OUTPUT, GPIO_PULL_DOWN, GPIO_PULL_NONE, HOST_START, HOST_STOP, IRQ_BOTH,
    PKT_HEADROOM, PKT_MAX,
};

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

const EV_CHECK: u16 = 0x7e00;
const EV_SUMMARY: u16 = 0x7eff;
const EV_ECHO: u16 = 0x7e10;
const EV_PKT: u16 = 0x7e11;

/// This module's own component and methods: see `API.md`.
pub const C_TEST: i32 = 2;
const M_ECHO: i32 = 1;
const M_INFO: i32 = 2;
const M_ONESHOT: i32 = 10;
const M_TICK: i32 = 11;
const M_IRQ: i32 = 12;
const M_MASKED: i32 = 13;
const M_TIMEOUT: i32 = 14;
const M_PKT_ECHO: i32 = 20;
const M_PKT_REPLY: i32 = 21;
const M_PKT_KEEP: i32 = 22;
const M_PKT_DROP: i32 = 23;
const M_MEM_LEAK: i32 = 24;

const MODE_SELFTEST: u8 = 0;
const MODE_SERVE: u8 = 1;
const MODE_ALARM: u8 = 2;
const MODE_GPIO: u8 = 3;

/// Bytes of state the image asks for (`mkmod` in Cargo.toml). An all-zero `State` is the initial state.
pub const STATE_BYTES: usize = 64;

#[derive(Default)]
pub struct State {
    mode: u8,
    done_oneshot: u8,
    irq_seen: u8,
    n: u16,
    pass: i16,
    fail: i16,
    a: i32,
    b: i32,
    periodic: i32,
    ticks: u32,
    calls: u32,
    kept: u32,
    t0: u64,
}

/// A line of text built in place: no `format!`, so a check costs no allocation.
struct Line {
    buf: [u8; 48],
    len: usize,
}

impl Line {
    fn new(name: &str) -> Line {
        let mut l = Line { buf: [0; 48], len: 0 };
        l.text(&name[..name.len().min(28)]);
        l
    }

    fn text(&mut self, s: &str) {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
    }

    fn int(&mut self, v: i64) {
        let mut digits = [0u8; 20];
        let (mut n, neg) = (0, v < 0);
        let mut u = v.unsigned_abs();
        loop {
            digits[n] = b'0' + (u % 10) as u8;
            n += 1;
            u /= 10;
            if u == 0 { break; }
        }
        if neg { self.text("-"); }
        while n > 0 {
            n -= 1;
            let d = [digits[n]];
            self.text(core::str::from_utf8(&d).unwrap_or("?"));
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl State {
    fn check(&mut self, hw: &Hw, name: &str, ok: bool, code: i32) {
        let mut l = Line::new(name);
        if ok {
            l.text(" ok");
            self.pass += 1;
        } else {
            l.text(" FAIL ");
            l.int(code as i64);
            self.fail += 1;
            hw.log(l.bytes()); // a failure is also a log line: the event list keeps only the last few
        }
        hw.event(C_TEST, EV_CHECK + self.n, l.bytes());
        self.n += 1;
    }

    /// The summary event; the result of the module: 0 when everything passed.
    fn finish(&mut self, hw: &Hw) -> i32 {
        let mut l = Line::new("pass=");
        l.int(self.pass as i64);
        l.text(" fail=");
        l.int(self.fail as i64);
        hw.event(C_TEST, EV_SUMMARY, l.bytes());
        hw.log(l.bytes());
        if self.fail != 0 { -(self.fail as i32) } else { xip_abi::RET_DONE as i32 }
    }

    fn cbor_checks(&mut self, hw: &Hw) {
        let mut buf = [0u8; 48];
        let mut w = Writer::new(&mut buf);
        w.map(4).uint(1).int(-68).uint(2).text("ok").uint(3).bytes(&[1, 2, 3]).uint(300).uint(0x12_3456_789a);
        let enc = w.finish();
        self.check(hw, "cbor-encode", matches!(enc, Some(e) if e[0] == 0xa4 && e[1] == 0x01 && e[2] == 0x38 && e[3] == 67), 0);
        let e = enc.unwrap_or(&[]);
        self.check(hw, "cbor-int", cbor::get_int(e, 1) == Ok(Some(-68)), 0);
        self.check(hw, "cbor-text", cbor::get_text(e, 2) == Ok(Some(&b"ok"[..])), 0);
        self.check(hw, "cbor-u64", cbor::get_int(e, 300) == Ok(Some(0x12_3456_789a)), 0);
        self.check(hw, "cbor-absent", cbor::get_int(e, 9) == Ok(None), 0);
        self.check(hw, "cbor-wrong-type", cbor::get_int(e, 2).is_err(), 0);
        self.check(hw, "cbor-truncated", e.len() > 3 && cbor::get_int(&e[..e.len() - 3], 300).is_err(), 0);
        let mut small = [0u8; 4];
        let mut w = Writer::new(&mut small);
        w.text("too long for four");
        self.check(hw, "cbor-overflow", w.finish().is_none(), 0);
    }

    /// Packets: a handle is valid until the packet leaves, a stale or zero handle is refused, `send` consumes the handle even when
    /// it fails, a module holds only a few.
    fn pkt_checks(&mut self, hw: &Hw) {
        let info = hw.pkt_info();
        self.check(hw, "pkt-info", matches!(info, Ok((h, m)) if h >= PKT_HEADROOM && m >= 1), 0);
        let p = hw.pkt_alloc();
        self.check(hw, "pkt-alloc", matches!(p, Ok(h) if h != 0), p.err().unwrap_or(0));
        let p = p.unwrap_or(0);
        let buf = hw.pkt_buf(p);
        let ok = matches!(buf, Ok((ptr, room)) if !ptr.is_null() && room >= 1 && room <= PKT_MAX as usize);
        self.check(hw, "pkt-buf", ok, buf.err().unwrap_or(0));
        if let Ok((ptr, room)) = buf {
            if room >= 8 {
                // SAFETY: the host lent `room` writable bytes at `ptr` until the packet is sent or freed
                let s = unsafe { core::slice::from_raw_parts_mut(ptr, 8) };
                for (i, b) in s.iter_mut().enumerate() { *b = 0xa0 + i as u8; }
                self.check(hw, "pkt-write", s[7] == 0xa7, 0);
            }
        }
        self.check(hw, "pkt-free", hw.pkt_free(p) == 0, 0);
        self.check(hw, "pkt-stale-free", hw.pkt_free(p) == ERR_ARG, 0);
        self.check(hw, "pkt-stale-buf", hw.pkt_buf(p) == Err(ERR_ARG), 0);
        self.check(hw, "pkt-zero", hw.pkt_free(0) == ERR_ARG && hw.pkt_keep(0) == ERR_ARG, 0);

        // the quota: a Vec of handles, in the module's own heap
        let mut held: Vec<u32> = Vec::new();
        while held.len() < 16 {
            match hw.pkt_alloc() {
                Ok(h) => held.push(h),
                Err(_) => break,
            }
        }
        let over = if held.len() < 16 { hw.pkt_alloc().err() } else { None };
        self.check(hw, "pkt-quota", !held.is_empty() && held.len() < 16 && matches!(over, Some(ERR_BUSY) | Some(ERR_NOMEM)), held.len() as i32);
        for h in held.drain(..) { hw.pkt_free(h); }

        // send gives the handle up whether it works or not
        let p = hw.pkt_alloc().unwrap_or(0);
        let rs = hw.send(p, PKT_MAX as usize + 1, ADDR_MAIN, C_TEST, M_PKT_REPLY, 1);
        self.check(hw, "pkt-send-big", p != 0 && rs == ERR_SIZE && hw.pkt_free(p) == ERR_ARG, rs);
        let p = hw.pkt_alloc().unwrap_or(0);
        let rs = hw.send(p, 4, 60000, C_TEST, M_PKT_REPLY, 1);
        self.check(hw, "pkt-send-nowhere", p != 0 && rs == ERR_NOTFOUND && hw.pkt_free(p) == ERR_ARG, rs);
        let p = hw.pkt_alloc().unwrap_or(0);
        let rs = hw.send(p, 4, ADDR_MAIN, C_TEST, M_PKT_REPLY, 2); // works or finds no hook: either way it is gone
        self.check(hw, "pkt-send-consumes", p != 0 && (rs == 0 || rs == ERR_NOTFOUND) && hw.pkt_free(p) == ERR_ARG, rs);
    }

    /// The managed heap: the host adds up what is taken and given back, the quota is a limit, the balance must be zero at stop.
    fn mem_checks(&mut self, hw: &Hw) {
        let (a, b) = (hw.mem_alloc(100), hw.mem_alloc(100));
        self.check(hw, "mem-alloc", !a.is_null() && !b.is_null() && a != b, 0);
        if !a.is_null() {
            // SAFETY: 100 bytes just allocated
            let s = unsafe { core::slice::from_raw_parts_mut(a, 100) };
            for (i, x) in s.iter_mut().enumerate() { *x = i as u8; }
            self.check(hw, "mem-rw", s[99] == 99, 0);
        }
        self.check(hw, "mem-free", hw.mem_free(a) == 0, 0);
        self.check(hw, "mem-null", hw.mem_free(core::ptr::null_mut()) == ERR_ARG, 0);
        self.check(hw, "mem-zero", hw.mem_alloc(0).is_null(), 0);
        self.check(hw, "mem-huge", hw.mem_alloc(1 << 24).is_null(), 0);
        self.check(hw, "mem-free-b", hw.mem_free(b) == 0, 0);
    }

    /// `alloc` over the host heap: the global allocator and an explicit one, a failure that is an error, and that everything is
    /// given back (the module would stop with `ERR_LEAK` otherwise: `tests/device_test.py` stops it).
    fn alloc_checks(&mut self, hw: &Hw) {
        {
            let mut v: Vec<u32> = Vec::new();
            for i in 0..100 { v.push(i); }
            let mut s = String::new();
            s.push_str("alloc");
            s.push_str(" ok");
            let b = Box::new(v.len() as u32);
            self.check(hw, "alloc-global", v.iter().sum::<u32>() == 4950 && s == "alloc ok" && *b == 100, 0);
        }
        {
            let mut e: Vec<u8, _> = Vec::new_in(hw.allocator());
            e.extend_from_slice(b"explicit");
            let mut grown: Vec<u16, _> = Vec::new_in(hw.allocator());
            for i in 0..200 { grown.push(i); }
            self.check(hw, "alloc-explicit", e == b"explicit" && grown.iter().map(|x| *x as u32).sum::<u32>() == 19900, 0);
        }
        let mut huge: Vec<u8> = Vec::new();
        self.check(hw, "alloc-fail", huge.try_reserve(1 << 24).is_err(), 0);
        let mut aligned: Vec<u64> = Vec::with_capacity(4);
        aligned.push(1);
        self.check(hw, "alloc-align", aligned.as_ptr() as usize % 8 == 0, 0);
    }

    fn selftest(&mut self, hw: &Hw) -> i32 {
        self.check(hw, "abi", hw.abi_version() == xip_abi::ABI_VERSION, hw.abi_version() as i32);
        self.check(hw, "log", hw.log(b"mod_rtest log") == 0, 0);
        let t0 = hw.now_ms();
        self.check(hw, "time", hw.now_ms() >= t0, 0);

        self.pkt_checks(hw);
        self.mem_checks(hw);
        self.alloc_checks(hw);

        // not a request: nothing to answer; connectivity is a plain query
        let plain = CallView { component: C_TEST, method: 0, value: 0, flags: 0, data: &[], pkt: 0, from: 0, id: 0 };
        self.check(hw, "reply-no-request", hw.reply(&plain, b"x") == ERR_ARG, 0);

        // settings: own namespace, round trip, overwrite, erase, size error
        {
            let (key, val, mut got) = (b"t", [1u8, 2, 3, 4], [0u8; 8]);
            self.check(hw, "setting-set", hw.set_setting(key, &val) == 0, 0);
            let r = hw.get_setting(key, &mut got);
            self.check(hw, "setting-get", r == Ok(4) && got[..4] == val, r.err().unwrap_or(0));
            let r = hw.get_setting(key, &mut got[..2]);
            self.check(hw, "setting-small", r == Err(ERR_SIZE), r.err().unwrap_or(0));
            self.check(hw, "setting-erase", hw.set_setting(key, &[]) == 0, 0);
            let r = hw.get_setting(key, &mut got);
            self.check(hw, "setting-gone", r == Err(ERR_NOTFOUND), r.err().unwrap_or(0));
            self.check(hw, "setting-key", hw.get_setting(b"0123456789abcdefg", &mut got) == Err(ERR_ARG), 0);
        }

        // alarms: argument validation, set and cancel (the firing is the "alarm" mode)
        self.check(hw, "alarm-bad-flags", hw.alarm_set(10, 0, 0, 0x80, C_TEST, M_ONESHOT) == ERR_ARG, 0);
        let al = hw.alarm_set(60000, 0, 0, ALARM_EXACT, C_TEST, M_ONESHOT);
        self.check(hw, "alarm-set", al >= 0, al);
        self.check(hw, "alarm-cancel", hw.alarm_cancel(al) == 0 && hw.alarm_cancel(al) == ERR_NOTFOUND, 0);

        // argument validation: rejected before touching hardware
        self.check(hw, "gpio-bad-pin", hw.gpio_config(500, GPIO_OUTPUT, GPIO_PULL_NONE, 0) == ERR_ARG, 0);
        self.check(hw, "gpio-unowned", hw.gpio_read(0) < 0, 0);
        self.check(hw, "irq-bad-pin", hw.irq_register(-1, xip_abi::IRQ_RISING, C_TEST, M_IRQ) < 0, 0);
        self.check(hw, "irq-unknown", hw.irq_unregister(0) == ERR_NOTFOUND, 0);
        let mut bad = SpiConfig::new(2);
        bad.mode = 9;
        self.check(hw, "spi-bad-config", hw.spi_open(&bad) < 0, 0);
        let mut rx = [0u8; 1];
        self.check(hw, "spi-bad-handle", hw.spi_transfer(7, b"x", &mut rx) < 0 && hw.spi_close(7) < 0, 0);
        self.check(hw, "i2c-bad-args", hw.i2c_transfer(0, 1, 2, 100_000, 0x10, &[], &mut [], 10) < 0, 0);
        self.check(hw, "adc-bad-pin", hw.adc_read(500, 3300).is_err(), 0);

        // events: messages to the application
        self.check(hw, "event", hw.event(C_TEST, EV_ECHO, &[1, 0, 0, 0, 0, 0, 0, 0]) == 0 && hw.event(C_TEST, EV_ECHO, &[]) == 0, 0);

        self.cbor_checks(hw);
        self.finish(hw)
    }

    /// `alarm`: a 30 ms one-shot and a 10 ms periodic alarm that cancels itself after 3 ticks.
    fn alarm_start(&mut self, hw: &Hw) -> i32 {
        self.t0 = hw.now_ms();
        let one = hw.alarm_set(30, 0, 0, ALARM_EXACT, C_TEST, M_ONESHOT);
        self.periodic = hw.alarm_set(10, 0, 10, ALARM_EXACT, C_TEST, M_TICK);
        self.check(hw, "alarms-set", one >= 0 && self.periodic >= 0 && one != self.periodic, one);
        xip_abi::RET_CONTINUE as i32
    }

    fn alarm_fired(&mut self, hw: &Hw, call: &CallView) -> i32 {
        let dt = hw.now_ms() - self.t0;
        if call.method == M_ONESHOT {
            self.done_oneshot = 1;
            self.check(hw, "oneshot", (25..500).contains(&dt) && call.value >= 0, dt as i32);
        } else {
            self.ticks += 1;
            if self.ticks == 3 {
                self.check(hw, "periodic", (25..500).contains(&dt), dt as i32);
                self.check(hw, "periodic-cancel", hw.alarm_cancel(self.periodic) == 0, 0);
            }
        }
        if self.done_oneshot != 0 && self.ticks >= 3 { self.finish(hw) } else { xip_abi::RET_CONTINUE as i32 }
    }

    /// `gpio A B`: A drives, B reads; B's edges arrive as `M_IRQ` calls.
    fn gpio_start(&mut self, hw: &Hw, request: &[u8]) -> i32 {
        let mut pos = 4;
        let (a, b) = (parse_int(request, &mut pos), parse_int(request, &mut pos));
        if a < 0 || b < 0 { return -1; }
        self.a = a;
        self.b = b;
        self.check(hw, "cfg-a", hw.gpio_config(a, GPIO_OUTPUT, GPIO_PULL_NONE, 0) == 0, 0);
        self.check(hw, "cfg-b", hw.gpio_config(b, GPIO_INPUT, GPIO_PULL_DOWN, 0) == 0, 0);
        hw.gpio_write(a, 1);
        self.check(hw, "high", hw.gpio_read(b) == 1, 0);
        hw.gpio_write(a, 0);
        self.check(hw, "low", hw.gpio_read(b) == 0, 0);
        self.check(hw, "irq-register", hw.irq_register(b, IRQ_BOTH, C_TEST, M_IRQ) == 0, 0);
        hw.alarm_set(150, 0, 0, ALARM_EXACT, C_TEST, M_TIMEOUT);
        hw.gpio_write(a, 1); // the edge comes back as an M_IRQ call
        xip_abi::RET_CONTINUE as i32
    }

    fn gpio_irq(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if self.irq_seen == 0 {
            self.irq_seen = 1;
            self.check(hw, "irq-event", call.value == 1, call.value);
            self.check(hw, "irq-disable", hw.irq_enable(self.b, false) == 0, 0);
            hw.gpio_write(self.a, 0); // masked: must stay quiet
            hw.alarm_set(50, 0, 0, ALARM_EXACT, C_TEST, M_MASKED);
        } else {
            self.irq_seen = self.irq_seen.saturating_add(1);
        }
        xip_abi::RET_CONTINUE as i32
    }

    fn gpio_masked(&mut self, hw: &Hw) -> i32 {
        self.check(hw, "irq-masked", self.irq_seen == 1, self.irq_seen as i32);
        self.check(hw, "irq-unregister", hw.irq_unregister(self.b) == 0, 0);
        self.check(hw, "other-owner", hw.gpio_write(self.a, 1) == 0, 0);
        self.finish(hw)
    }

    /// `spi SCK MISO MOSI CS [BUS]`: MOSI wired to MISO, the bytes come back.
    fn spi_loop(&mut self, hw: &Hw, request: &[u8]) -> i32 {
        let mut pos = 3;
        let (sck, miso, mosi, cs, bus) = (
            parse_int(request, &mut pos), parse_int(request, &mut pos), parse_int(request, &mut pos),
            parse_int(request, &mut pos), parse_int(request, &mut pos),
        );
        if sck < 0 || miso < 0 || mosi < 0 || cs < 0 { return -1; }
        let mut c = SpiConfig::new(if bus < 0 { 2 } else { bus });
        (c.sck, c.miso, c.mosi, c.cs, c.hz) = (sck, miso, mosi, cs, 1_000_000);
        let s = hw.spi_open(&c);
        self.check(hw, "open", s >= 0, s);
        if s >= 0 {
            let mut tx = [0u8; 16];
            for (i, b) in tx.iter_mut().enumerate() { *b = 0xa0 + i as u8; }
            let mut rx = [0u8; 16];
            let rc = hw.spi_transfer(s, &tx, &mut rx);
            self.check(hw, "transfer", rc == 0, rc);
            self.check(hw, "loopback", tx == rx, (rx[0] as i32) << 8 | rx[1] as i32); // on failure: the first two bytes that came back
            self.check(hw, "close", hw.spi_close(s) == 0, 0);
        }
        self.finish(hw)
    }

    fn request(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.method == M_ECHO {
            let text = cbor::get_text(call.data, 1).ok().flatten().unwrap_or(&[]);
            hw.event(C_TEST, EV_ECHO, text);
            if call.wants_response() {
                let mut buf = [0u8; 96];
                let mut w = Writer::new(&mut buf);
                w.map(1).uint(1);
                if let Ok(t) = core::str::from_utf8(text) { w.text(t); } else { w.text(""); }
                if let Some(e) = w.finish() { hw.reply(call, e); }
            }
        } else if call.method == M_INFO && call.wants_response() {
            let mut buf = [0u8; 16];
            let mut w = Writer::new(&mut buf);
            w.map(2).uint(1).uint(self.calls as u64).uint(2).uint(self.mode as u64);
            if let Some(e) = w.finish() { hw.reply(call, e); }
        }
        xip_abi::RET_CONTINUE as i32
    }
}

fn parse_int(s: &[u8], pos: &mut usize) -> i32 {
    while *pos < s.len() && s[*pos] == b' ' { *pos += 1; }
    let (mut v, mut any) = (0i32, false);
    while *pos < s.len() && s[*pos].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((s[*pos] - b'0') as i32);
        *pos += 1;
        any = true;
    }
    if any { v } else { -1 }
}

impl State {
    /// The traffic of the manifest's two ports, both an echo (the end-to-end test of a link, whatever the link): a **datagram** (port 3, a packet of
    /// a link with `CALL_LINK`) goes back to `from` with the token it came with, in the same packet; a **stream** (stream port 3, TCP 5228: a
    /// socket event) is read and written back. `CALL_LINK_EVENT`s of a link need no answer.
    fn port_traffic(&mut self, hw: &Hw, call: &CallView) -> i32 {
        const CONTINUE: i32 = xip_abi::RET_CONTINUE as i32;
        if call.flags & xip_abi::CALL_LINK != 0 {
            if call.flags & xip_abi::CALL_LINK_EVENT == 0 && call.pkt != 0 {
                // a link takes only a packet the module owns: keep the lent one, then pass it on (the zero copy path of a relay)
                if hw.pkt_keep(call.pkt) == 0 {
                    let _ = hw.send(call.pkt, call.data.len(), call.from, 0, 0, call.id);
                }
            }
            return CONTINUE;
        }
        let handle = call.id as u32;
        match call.value as u32 {
            xip_abi::SOCKET_EVENT_READABLE => {
                let mut buf = [0u8; 256];
                loop {
                    let n = hw.socket_read(handle, &mut buf);
                    if n == ERR_AGAIN { break; }
                    if n <= 0 { hw.socket_close(handle); break; }
                    let mut at = 0;
                    while at < n as usize {
                        let w = hw.socket_write(handle, &buf[at..n as usize]);
                        if w <= 0 { break; } // TODO: flow control; the echo drops what the socket does not take
                        at += w as usize;
                    }
                }
            }
            xip_abi::SOCKET_EVENT_CLOSED | xip_abi::SOCKET_EVENT_EOF => { hw.socket_close(handle); }
            _ => {}
        }
        CONTINUE
    }
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        self.calls += 1;
        if call.component == 0 {
            if call.is_host(HOST_STOP) {
                hw.event(C_TEST, EV_SUMMARY, b"stopped");
                return 0;
            }
            if !call.is_host(HOST_START) { return xip_abi::RET_CONTINUE as i32; } // NETWORK, WAKE: not used
            let d = call.data;
            if d.starts_with(b"serve") {
                self.mode = MODE_SERVE;
                hw.log(b"mod_rtest serving");
                return xip_abi::RET_CONTINUE as i32;
            }
            if d.starts_with(b"alarm") { self.mode = MODE_ALARM; return self.alarm_start(hw); }
            if d.starts_with(b"gpio ") { self.mode = MODE_GPIO; return self.gpio_start(hw, d); }
            if d.starts_with(b"spi ") { return self.spi_loop(hw, d); }
            self.mode = MODE_SELFTEST;
            return self.selftest(hw);
        }
        if call.component != C_TEST { return xip_abi::RET_CONTINUE as i32; }
        if call.method == 0 { return self.port_traffic(hw, call); }
        match call.method {
            M_ONESHOT | M_TICK => self.alarm_fired(hw, call),
            M_IRQ => self.gpio_irq(hw, call),
            M_MASKED => self.gpio_masked(hw),
            M_TIMEOUT => {
                if self.irq_seen != 0 { return xip_abi::RET_CONTINUE as i32; }
                self.check(hw, "irq-event", false, -1);
                self.finish(hw)
            }
            M_PKT_ECHO => {
                if call.pkt != 0 {
                    if let Ok((ptr, _)) = hw.pkt_buf(call.pkt) {
                        // SAFETY: the packet is lent to this call and `data` is its first `len` bytes
                        if !call.data.is_empty() { unsafe { *ptr = !*ptr; } }
                    }
                    let _ = hw.send(call.pkt, call.data.len(), call.from, C_TEST, M_PKT_REPLY, call.id);
                }
                xip_abi::RET_CONTINUE as i32
            }
            M_PKT_REPLY => {
                hw.event(C_TEST, EV_PKT, b"reply");
                xip_abi::RET_CONTINUE as i32
            }
            M_PKT_KEEP => {
                if call.pkt != 0 && self.kept == 0 && hw.pkt_keep(call.pkt) == 0 { self.kept = call.pkt; }
                xip_abi::RET_CONTINUE as i32
            }
            M_MEM_LEAK => {
                let _ = hw.mem_alloc(64);
                xip_abi::RET_CONTINUE as i32
            }
            M_PKT_DROP => {
                if self.kept != 0 {
                    hw.pkt_free(self.kept);
                    self.kept = 0;
                }
                xip_abi::RET_CONTINUE as i32
            }
            _ => self.request(hw, call),
        }
    }
}

const _: () = assert!(core::mem::size_of::<State>() <= STATE_BYTES);

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(feature = "sim")]
export!();
