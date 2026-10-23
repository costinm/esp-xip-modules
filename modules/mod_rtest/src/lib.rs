//! `mod_rtest`: the device test of the ABI in Rust, the device test module (component 2, the `test` slot). The tests (`tests/device_test.py`, the console) drive it. It calls every entry of the host table and reports one event per check; on top of the C module it uses `alloc` through
//! both allocators of `xip-abi` (the global one and an explicit one) and checks that a failed allocation is an error, not an abort.
//!
//! The start request picks the mode: empty or `selftest` (every check that needs no wiring; finishes with 0 when all pass),
//! `serve`, `alarm`, `async` (owned event/datagram echo), `async-alarm`,
//! `async-yield`, `async-pause` (device overflow bench), `gpio A B`, `spi SCK MISO MOSI CS [BUS]`. Events: `0x7e00 + n` per check, text `"<name> ok"` or
//! `"<name> FAIL <code>"`; `0x7eff` the summary.
//!
//! Like every module it never waits and has no writable statics: what it remembers is the state block, and every `Vec` is
//! freed before stop (a block still held when the module stops is `ERR_LEAK`).
//! Async mode intentionally retains owned calls/future allocations between handlers.
#![no_std]
#![feature(allocator_api)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use xip_abi::cbor::{self, Writer};
use xip_abi::{
    event_kind, CallView, Hw, Module, PowerInfo, SpiConfig, WifiIntent, WifiMonitor, ADDR_MAIN, ALARM_EXACT, CALL_LINK,
    CALL_LINK_EVENT, ERR_AGAIN, ERR_ARG, ERR_BUSY, ERR_NOMEM, ERR_NOTFOUND, ERR_SIZE, ERR_UNSUPPORTED, GPIO_INPUT,
    GPIO_OUTPUT, GPIO_PULL_DOWN, GPIO_PULL_NONE, HOST_START, HOST_STOP, HOST_WIFI, IRQ_BOTH, LINK_EVENT_TX_DONE,
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
const MODE_ASYNC: u8 = 4;
/// `now [fast] [ap]`: the two-board bench of the `espnow` link; `mon`: the promiscuous-receive bench.
const MODE_NOW: u8 = 5;
const MODE_MON: u8 = 6;

/// The bench methods (30..): the frame and event handlers of the `espnow` / `promisc` links, the bench alarm and its guard.
const M_NOW_FRAME: i32 = 30;
const M_NOW_TXDONE: i32 = 31;
const M_NOW_TICK: i32 = 32;
const M_NOW_GUARD: i32 = 33;
const M_MON_FRAME: i32 = 34;
const M_MON_TICK: i32 = 35;
const M_MON_GUARD: i32 = 36;
const M_NOW_WAIT: i32 = 37;

/// The bench channel: both boards unassociated, pinned here, so a pair meets whatever the lab around them does.
const NOW_CHANNEL: u8 = 6;
const NOW_GUARD_MS: u32 = 15_000;
const MON_GUARD_MS: u32 = 15_000;
const BENCH_PRINT: u32 = 6;
/// `NOWP` ping, `NOWQ` pong: only a ping is answered, so a pair of boards does not talk forever.
const PING: [u8; 4] = *b"NOWP";
const PONG: [u8; 4] = *b"NOWQ";

/// Bytes of state the image asks for (`mkmod` in Cargo.toml). An all-zero `State` is the initial state.
pub const STATE_BYTES: usize = 512;

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
    task: xip_abi::task::Task,
    // the `now` and `mon` benches: the link addresses (0: the bench has not started yet), the counters, and the wifi to restore
    now_link: i32,
    mon_link: i32,
    mon_begun: bool,
    now_guard: u32,
    /// 500 ms polls left for the `now sta` variant's wait for the station to connect (no guard alarm is armed before the bench begins).
    now_wait_left: u32,
    now_interval: u32,
    now_seq: u32,
    now_sent: u32,
    now_busy: u32,
    now_txok: u32,
    now_txerr: u32,
    now_rx: u32,
    now_pong: u32,
    now_echo: u32,
    now_peer: u64,
    now_printed: u32,
    now_rx_only: bool,
    now_size: u32,
    now_dest: u64,
    now_valid_ping: u32,
    now_bad: u32,
    now_min_rx: u32,
    now_max_rx: u32,
    mon_frames: u32,
    mon_base: u32,
    /// The channel the monitor pins (0: leave the radio's channel, the `mon sta` variant).
    mon_channel: u8,
    orig: WifiIntent,
    orig_generation: u32,
}

    /// A line of text built in place: no `format!`, so a check costs no allocation.
    struct Line {
        buf: [u8; 96],
        len: usize,
    }

impl Line {
    fn new(name: &str) -> Line {
        let mut l = Line { buf: [0; 96], len: 0 };
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

    fn hex(&mut self, bytes: &[u8]) {
        const H: &[u8; 16] = b"0123456789abcdef";
        for b in bytes {
            let d = [H[(b >> 4) as usize], H[(b & 15) as usize]];
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
            let mut buf = [0u8; 192];
            if let Some(e) = self.info(&mut buf) { hw.reply(call, e); }
        }
        xip_abi::RET_CONTINUE as i32
    }
}

/// Full-payload integrity includes declared length, catching truncation too.
fn now_byte(seq: u32, i: usize) -> u8 {
    (seq.rotate_left((i % 32) as u32) ^ (i as u32).wrapping_mul(73) ^ ((i as u32) >> 8)) as u8
}
fn fill_now(data: &mut [u8], magic: [u8; 4], seq: u32) {
    let len = data.len() as u32;
    data[..4].copy_from_slice(&magic);
    data[4..8].copy_from_slice(&seq.to_le_bytes());
    data[8..12].copy_from_slice(&len.to_le_bytes());
    for (i, b) in data.iter_mut().enumerate().skip(12) { *b = now_byte(seq, i); }
}
fn valid_now(data: &[u8]) -> bool {
    if data.len() < 12 { return false; }
    let seq = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let len = u32::from_le_bytes(data[8..12].try_into().unwrap());
    len as usize == data.len() && data.iter().enumerate().skip(12).all(|(i, b)| *b == now_byte(seq, i))
}

/// The bench's guard duration from a `secs=N` word of the start request; the mode's default otherwise.
fn guard_ms(request: &[u8]) -> u32 {
    for at in 0..request.len() {
        if request[at..].starts_with(b"secs=") {
            let (mut v, mut any) = (0u32, false);
            for b in &request[at + 5..] {
                if !b.is_ascii_digit() { break; }
                v = v.saturating_mul(10).saturating_add((*b - b'0') as u32);
                any = true;
            }
            if any { return v * 1000; }
        }
    }
    0
}

fn parse_int(s: &[u8], pos: &mut usize) -> i32 {    while *pos < s.len() && s[*pos] == b' ' { *pos += 1; }
    let (mut v, mut any) = (0i32, false);
    while *pos < s.len() && s[*pos].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((s[*pos] - b'0') as i32);
        *pos += 1;
        any = true;
    }
    if any { v } else { -1 }
}

impl State {
    fn info<'a>(&self, buf: &'a mut [u8]) -> Option<&'a [u8]> {
        let mut w = Writer::new(buf);
        w.map(if self.mode == MODE_NOW { 16 } else { 2 })
            .uint(1).uint(self.calls as u64).uint(2).uint(self.mode as u64);
        if self.mode != MODE_NOW { return w.finish(); }
        w.uint(3).uint(self.now_size as u64).uint(4).uint(self.now_sent as u64)
            .uint(5).uint(self.now_busy as u64).uint(6).uint(self.now_txerr as u64)
            .uint(7).uint(self.now_valid_ping as u64).uint(8).uint(self.now_pong as u64)
            .uint(9).uint(self.now_echo as u64).uint(10).uint(self.now_bad as u64)
            .uint(11).uint(self.now_min_rx as u64).uint(12).uint(self.now_max_rx as u64)
            .uint(13).uint(self.now_txok as u64).uint(14).int(self.now_link as i64)
            .uint(15).uint(self.now_peer).uint(16).uint(self.now_dest);
        w.finish()
    }

    /// The traffic of the manifest's two ports, both an echo (the end-to-end test of a link, whatever the link): a **datagram** (port 3, a packet of
    /// a link with `CALL_LINK`) goes back to `from` with the token it came with, in the same packet; a **stream** (stream port 3, TCP 5228: a
    /// socket event) is read and written back. `CALL_LINK_EVENT`s of a link need no answer.
    fn port_traffic(&mut self, hw: &Hw, call: &CallView) -> i32 {
        const CONTINUE: i32 = xip_abi::RET_CONTINUE as i32;
        if call.flags & xip_abi::CALL_LINK != 0 {
            if self.mode == MODE_NOW && call.flags & CALL_LINK_EVENT == 0 {
                if cbor::get_int(call.data, 1).ok().flatten() == Some(C_TEST as i64)
                    && cbor::get_int(call.data, 2).ok().flatten() == Some(M_INFO as i64) {
                    let mut fields = [0u8; 192];
                    if let Some(info) = self.info(&mut fields) {
                        let id = cbor::get_int(call.data, 3).ok().flatten().unwrap_or(0);
                        let mut answer = [0u8; 240];
                        let mut w = Writer::new(&mut answer);
                        w.map(4).uint(1).int(C_TEST as i64).uint(2).int(M_INFO as i64)
                            .uint(3).int(id).uint(6).raw(info);
                        if let Some(data) = w.finish() { hw.message(call.from, 0, 0, call.id, data); }
                    }
                }
                return CONTINUE;
            }
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

impl State {
    /// The radio state both benches want: on, unassociated, pinned to `NOW_CHANNEL` (HT20), with `now` set (what
    /// starts the `espnow` link). Two boards that both apply it meet on the same channel, with the link up and
    /// nothing else on the air of their own. `ap` selects the APSTA comparison;
    /// NOW-only receives using the owner's connectionless wake-window vote.
    fn bench_intent(&self, ap: bool) -> WifiIntent {
        WifiIntent { radio: 1, sta: 0, ap: ap as u8, now: 1, channel: NOW_CHANNEL, ..WifiIntent::default() }
    }

    fn wifi_get_intent(&mut self, hw: &Hw) -> WifiIntent {
        self.orig = WifiIntent::default();
        if let Some(get) = hw.wifi().and_then(|w| w.get) {
            let mut generation = 0u32;
            // SAFETY: the host's own table call
            unsafe { get(hw.ctx(), &mut self.orig, &mut generation) };
            self.orig_generation = generation;
        }
        self.orig
    }

    fn wifi_set_intent(&self, hw: &Hw, want: &WifiIntent) -> i32 {
        let Some(set) = hw.wifi().and_then(|w| w.set) else { return ERR_UNSUPPORTED };
        let mut generation = 0u32;
        // SAFETY: the host's own table call
        unsafe { set(hw.ctx(), want, &mut generation) }
    }

    fn monitor(&self, hw: &Hw, enable: bool) -> i32 {
        let Some(m) = hw.wifi().and_then(|w| w.monitor) else { return ERR_UNSUPPORTED };
        let channel = if enable { self.mon_channel } else { 0 };
        let req = WifiMonitor { enable: enable as u8, channel, ..WifiMonitor::default() };
        // SAFETY: the host's own table call
        unsafe { m(hw.ctx(), &req) }
    }

    /// The `power.info` promiscuous counters: (frames the filter kept, frames it dropped), cumulative since boot.
    fn power_promisc(&self, hw: &Hw) -> Option<(u32, u32)> {
        let p = hw.power()?;
        let info = p.info?;
        let mut i = PowerInfo::default();
        // SAFETY: the host's own table call
        if unsafe { info(hw.ctx(), &mut i) } != 0 { return None; }
        Some((i.promisc_frames, i.filter_dropped))
    }

    fn link_open(&self, hw: &Hw, name: &core::ffi::CStr, method: i32) -> i32 {
        let Some(open) = hw.link().and_then(|l| l.open) else { return ERR_UNSUPPORTED };
        // SAFETY: the host's own table call
        unsafe { open(hw.ctx(), name.as_ptr(), C_TEST, method) }
    }

    // ---- the `espnow` link bench (`start test now [fast]`) ----

    fn now_start(&mut self, hw: &Hw, request: &[u8]) -> i32 {
        self.now_interval = if request.windows(4).any(|w| w == b"fast") { 20 } else if request.windows(5).any(|w| w == b"paced") { 100 } else { 1000 };
        if request.windows(6).any(|w| w == b"listen") { self.now_interval = 0; }
        self.now_rx_only = request.windows(6).any(|w| w == b"rxonly");
        self.now_size = 64;
        self.now_dest = 0;
        for word in request.split(|b| *b == b' ') {
            if let Some(value) = word.strip_prefix(b"dest=") {
                let Some(dest) = core::str::from_utf8(value).ok().and_then(|v| v.parse().ok()) else { return ERR_ARG; };
                self.now_dest = dest;
                if self.now_dest == 0 { return ERR_ARG; }
            }
            if let Some(value) = word.strip_prefix(b"size=") {
                let mut pos = 0;
                let n = parse_int(value, &mut pos);
                let max = hw.pkt_info().map_or(0, |(_, max)| max);
                if n < 12 || n as u32 > max { return ERR_SIZE; }
                self.now_size = n as u32;
            }
        }
        self.now_valid_ping = 0; self.now_bad = 0; self.now_min_rx = 0; self.now_max_rx = 0;
        let now_ap = request.windows(2).any(|w| w == b"ap");
        self.now_guard = guard_ms(request);
        // the state survives stop/start: a run's counters must not leak into the next one
        self.now_seq = 0; self.now_sent = 0; self.now_busy = 0;
        self.now_txok = 0; self.now_txerr = 0; self.now_rx = 0;
        self.now_pong = 0; self.now_echo = 0; self.now_peer = 0; self.now_printed = 0;
        let orig = self.wifi_get_intent(hw);
        if request.windows(3).any(|w| w == b"sta") {
            // the radio as it is (an associated station, the AP's channel): the bench does not touch it.
            // The association may still be in progress: an action TX while the STA is joining is refused
            // by the driver, so the bench waits for the station to be connected first — on a budget of
            // the guard duration (no guard alarm is armed until the bench begins).
            self.now_wait_left = (if self.now_guard != 0 { self.now_guard } else { NOW_GUARD_MS }) / 500 + 1;
            self.wait_connected(hw, 500);
            return xip_abi::RET_CONTINUE as i32;
        }
        if orig.radio == 1 && orig.sta == 0 && orig.ap == now_ap as u8 && orig.now == 1 && orig.channel == NOW_CHANNEL {
            return self.now_begin(hw);
        }
        let want = self.bench_intent(now_ap);
        let rc = self.wifi_set_intent(hw, &want);
        self.check(hw, "espnow-intent", rc == 0, rc);
        if rc != 0 { return self.finish(hw); }
        xip_abi::RET_CONTINUE as i32
    }

    /// On the HOST_WIFI of the bench intent: subscribe to the `espnow` link, arm the tick and the guard.
    fn now_begin(&mut self, hw: &Hw) -> i32 {
        self.now_link = self.link_open(hw, c"espnow", M_NOW_FRAME);
        self.check(hw, "espnow-open", self.now_link >= xip_abi::ADDR_LINK as i32, self.now_link);
        if self.now_link < xip_abi::ADDR_LINK as i32 { return self.finish(hw); }
        hw.log(b"now: bench started");
        if self.now_interval != 0 { hw.alarm_set(200, 0, self.now_interval, ALARM_EXACT, C_TEST, M_NOW_TICK); }
        hw.alarm_set(if self.now_guard != 0 { self.now_guard } else { NOW_GUARD_MS }, 0, 0, ALARM_EXACT, C_TEST, M_NOW_GUARD);
        xip_abi::RET_CONTINUE as i32
    }

    fn now_tick(&mut self, hw: &Hw) {
        self.now_ping(hw, self.now_dest);
        // every 8th ping also goes unicast to the peer we last heard from: the hardware-ACK path
        if self.now_dest == 0 && self.now_peer != 0 && self.now_seq % 8 == 0 {
            self.now_ping(hw, self.now_peer);
        }
        // live progress, so a host watching the port sees the counters while a long bench runs
        if self.now_seq % 8 == 0 {
            let mut l = Line::new("now: tick sent=");
            l.int(self.now_sent as i64);
            l.text(" rx=");
            l.int(self.now_rx as i64);
            l.text(" echo=");
            l.int(self.now_echo as i64);
            l.text(" err=");
            l.int((self.now_txerr + self.now_busy) as i64);
            hw.log(l.bytes());
        }
    }

    /// Poll the station state every `delay` ms until it is connected, then start the bench; when the wait budget
    /// (`now_wait_left`) runs out, the bench ends with a failed check instead of polling forever.
    fn wait_connected(&mut self, hw: &Hw, delay: u32) {
        let mut st = xip_abi::WifiState { size: core::mem::size_of::<xip_abi::WifiState>() as u32, ..Default::default() };
        let connected = hw.wifi().and_then(|w| w.state).map_or(false, |state| unsafe { state(hw.ctx(), &mut st) } == 0 && st.sta == 2);
        if connected {
            self.now_begin(hw);
        } else if self.now_wait_left > 1 {
            self.now_wait_left -= 1;
            hw.alarm_set(delay, 0, 0, ALARM_EXACT, C_TEST, M_NOW_WAIT);
        } else {
            self.check(hw, "espnow-connect", false, -1);
            self.finish(hw);
        }
    }

    /// One frame out on the link (`dest` is a MAC token, 0 the broadcast), with the TX_DONE outcome back to M_NOW_TXDONE.
    fn now_ping(&mut self, hw: &Hw, dest: u64) {
        let Ok(pkt) = hw.pkt_alloc() else { self.now_busy += 1; return; };
        let Ok((ptr, room)) = hw.pkt_buf(pkt) else { hw.pkt_free(pkt); return; };
        let len = self.now_size as usize;
        if room < len { hw.pkt_free(pkt); self.now_txerr += 1; return; }
        self.now_seq = self.now_seq.wrapping_add(1);
        // Sole mutable pool lease; the C link writes only framing in headroom.
        fill_now(unsafe { core::slice::from_raw_parts_mut(ptr, len) }, PING, self.now_seq);
        let rc = hw.send(pkt, len, self.now_link as u32, C_TEST, M_NOW_TXDONE, dest);
        match rc {
            0 => self.now_sent += 1,
            // a busy link keeps the packet with us (the runtime's one send outcome that does not consume it)
            ERR_BUSY => { self.now_busy += 1; hw.pkt_free(pkt); }
            e => {
                self.now_txerr += 1;
                if self.now_printed < BENCH_PRINT {
                    self.now_printed += 1;
                    let mut l = Line::new("now: send failed ");
                    l.int(e as i64);
                    hw.log(l.bytes());
                }
            }
        }
    }

    /// A frame of the `espnow` link: a ping is answered with one unicast pong (a fresh packet, only pings are
    /// answered, so a pair never loops); a pong is only counted.
    fn now_frame(&mut self, hw: &Hw, call: &CallView) {
        if call.flags & CALL_LINK == 0 || call.flags & CALL_LINK_EVENT != 0 { return; }
        let data = call.data;
        if data.len() < 4 || (data[..4] != PING && data[..4] != PONG) { return; }
        // Ignore intact traffic from a preceding size row. A truncated packet
        // declaring the current row's size must still fail integrity below.
        if data.len() >= 12 && u32::from_le_bytes(data[8..12].try_into().unwrap()) != self.now_size { return; }
        self.now_rx += 1;
        if !valid_now(data) { self.now_bad += 1; return; }
        let len = data.len();
        self.now_min_rx = if self.now_min_rx == 0 { len as u32 } else { self.now_min_rx.min(len as u32) };
        self.now_max_rx = self.now_max_rx.max(len as u32);
        self.now_peer = call.id;
        if data[..4] == PING {
            self.now_valid_ping += 1;
            if self.now_rx_only { return; }
            let seq = u32::from_le_bytes(data[4..8].try_into().unwrap());
            if let Ok(p) = hw.pkt_alloc() {
                let rc = if let Ok((ptr, room)) = hw.pkt_buf(p) {
                    if room >= len {
                        // Independent semantic response from validated sequence/length.
                        fill_now(unsafe { core::slice::from_raw_parts_mut(ptr, len) }, PONG, seq);
                        hw.send(p, len, self.now_link as u32, 0, 0, call.id)
                    } else { hw.pkt_free(p); ERR_SIZE }
                } else { hw.pkt_free(p); ERR_NOMEM };
                if rc == 0 { self.now_echo += 1; }
                else { self.now_txerr += 1; if rc == ERR_BUSY { hw.pkt_free(p); } }
            }
        } else { self.now_pong += 1; }
    }

    /// The guard: the wifi as it was, the checks over the counters, the report, done.
    fn now_end(&mut self, hw: &Hw) -> i32 {
        let restore = self.wifi_set_intent(hw, &self.orig);
        if restore != 0 { self.check(hw, "espnow-restore", false, restore); }
        self.check(hw, "espnow-tx", self.now_sent > 0 && self.now_txerr == 0, self.now_txerr as i32);
        self.check(hw, "espnow-rx", self.now_rx > 0, self.now_rx as i32);
        self.check(hw, "espnow-echo", self.now_echo > 0, self.now_echo as i32);
        let mut l = Line::new("now: report sent=");
        l.int(self.now_sent as i64);
        l.text(" busy=");
        l.int(self.now_busy as i64);
        l.text(" txerr=");
        l.int(self.now_txerr as i64);
        hw.log(l.bytes());
        let mut l = Line::new("now: report rx=");
        l.int(self.now_rx as i64);
        l.text(" pong=");
        l.int(self.now_pong as i64);
        l.text(" echo=");
        l.int(self.now_echo as i64);
        hw.log(l.bytes());
        let mut l = Line::new("now: peer token ");
        l.int(self.now_peer as i64);
        hw.log(l.bytes());
        self.finish(hw)
    }

    // ---- the promiscuous-receive bench (`start test mon`) ----

    fn mon_start(&mut self, hw: &Hw, request: &[u8]) -> i32 {
        self.now_size = 64; self.now_dest = 0; self.now_rx_only = false;
        // `mon fast` also transmits: the board then hears its own frames in promiscuous receive, which
        // proves the frames leave the radio and shows their addresses as the driver put them on the air.
        // `mon sta` keeps the station association and monitors its channel instead of pinning the bench one.
        self.now_interval = if request.windows(4).any(|w| w == b"fast") { 20 } else { 0 };
        self.mon_channel = if request.windows(3).any(|w| w == b"sta") { 0 } else { NOW_CHANNEL };
        self.now_guard = guard_ms(request);
        if self.now_guard == 0 { self.now_guard = MON_GUARD_MS; }
        // the state survives stop/start: a run's counters must not leak into the next one
        self.mon_frames = 0; self.now_printed = 0;
        self.now_seq = 0; self.now_sent = 0; self.now_busy = 0;
        self.now_txok = 0; self.now_txerr = 0; self.now_rx = 0;
        self.now_pong = 0; self.now_echo = 0; self.now_peer = 0;
        let _ = self.wifi_get_intent(hw);
        self.mon_link = self.link_open(hw, c"promisc", M_MON_FRAME);
        self.check(hw, "promisc-open", self.mon_link >= xip_abi::ADDR_LINK as i32, self.mon_link);
        if self.mon_link < xip_abi::ADDR_LINK as i32 { return self.finish(hw); }
        self.mon_base = self.power_promisc(hw).map_or(0, |(frames, _)| frames);
        if self.mon_channel == 0
            || (self.orig.radio == 1 && self.orig.sta == 0 && self.orig.ap == 0 && self.orig.channel == NOW_CHANNEL)
        {
            return self.mon_begin(hw);
        }
        let want = self.bench_intent(false);
        let rc = self.wifi_set_intent(hw, &want);
        self.check(hw, "promisc-intent", rc == 0, rc);
        if rc != 0 { return self.finish(hw); }
        xip_abi::RET_CONTINUE as i32
    }

    /// On the HOST_WIFI of the bench intent: promiscuous receive on, the progress and guard alarms.
    fn mon_begin(&mut self, hw: &Hw) -> i32 {
        self.mon_begun = true;
        let rc = self.monitor(hw, true);
        self.check(hw, "promisc-on", rc == 0, rc);
        if rc != 0 { return self.mon_end(hw); }
        hw.log(b"mon: listening");
        if self.now_interval != 0 {
            // the transmit side of the self-reception: the `espnow` link, pings every `interval` ms
            self.now_link = self.link_open(hw, c"espnow", M_NOW_FRAME);
            let mut l = Line::new("mon: espnow link ");
            l.int(self.now_link as i64);
            hw.log(l.bytes());
            hw.alarm_set(500, 0, self.now_interval, ALARM_EXACT, C_TEST, M_NOW_TICK);
        }
        hw.alarm_set(2000, 0, 2000, ALARM_EXACT, C_TEST, M_MON_TICK);   // the progress report, every 2 s
        hw.alarm_set(self.now_guard, 0, 0, ALARM_EXACT, C_TEST, M_MON_GUARD);
        xip_abi::RET_CONTINUE as i32
    }

    /// A raw 802.11 frame the filter kept (the promisc link has no transmit): the type byte, the A3
    /// (address 3 — the field the early filter judges a frame's network by) and the length. Action
    /// frames (management subtype 0xd0) are each logged, however many: they are what the bench is about.
    fn mon_frame(&mut self, hw: &Hw, call: &CallView) {
        if call.flags & CALL_LINK == 0 || call.flags & CALL_LINK_EVENT != 0 { return; }
        self.mon_frames += 1;
        // `data` is a view into the packet the call lends: everything is read out before it is freed
        let len = call.data.len();
        let type_byte = if len == 0 { -1i64 } else { call.data[0] as i64 };
        let action = len > 0 && call.data[0] & 0xfc == 0xd0;
        let mut a3 = [0u8; 6];
        if len >= 22 { a3.copy_from_slice(&call.data[16..22]); }
        let rssi = xip_abi::rssi(call.value) as i64;
        let _ = hw.pkt_free(call.pkt);
        if action || self.now_printed < BENCH_PRINT {
            self.now_printed += 1;
            let mut l = Line::new("mon: frame type ");
            l.int(type_byte);
            l.text(" a3 ");
            if len >= 22 { l.hex(&a3); } else { l.text("short"); }
            l.text(" len ");
            l.int(len as i64);
            l.text(" rssi ");
            l.int(rssi);
            hw.log(l.bytes());
        }
    }

    /// The guard: promiscuous off, the counters, the wifi as it was, done.
    fn mon_end(&mut self, hw: &Hw) -> i32 {
        let _ = self.monitor(hw, false);
        let (kept, dropped) = self.power_promisc(hw).unwrap_or((0, 0));
        self.check(hw, "promisc-frames", self.mon_frames > 0, self.mon_frames as i32);
        let mut l = Line::new("mon: report frames=");
        l.int(self.mon_frames as i64);
        hw.log(l.bytes());
        let mut l = Line::new("mon: counters kept=");
        l.int(kept as i64);
        l.text(" base=");
        l.int(self.mon_base as i64);
        l.text(" drop=");
        l.int(dropped as i64);
        hw.log(l.bytes());
        let restore = self.wifi_set_intent(hw, &self.orig);
        if restore != 0 { self.check(hw, "promisc-restore", false, restore); }
        self.finish(hw)
    }
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        self.calls += 1;
        if self.mode == MODE_ASYNC {
            return self.task.handle(hw, call);
        }
        if call.component == 0 {
            if call.is_host(HOST_STOP) {
                if self.mode == MODE_MON { let _ = self.monitor(hw, false); }
                if self.mode == MODE_NOW || self.mode == MODE_MON { let _ = self.wifi_set_intent(hw, &self.orig); }
                hw.event(C_TEST, EV_SUMMARY, b"stopped");
                return 0;
            }
            if call.is_host(HOST_WIFI) {
                // the wanted radio state was committed: the benches begin here (the espnow / promisc links are up)
                if self.mode == MODE_NOW && self.now_link == 0 { return self.now_begin(hw); }
                if self.mode == MODE_MON && !self.mon_begun { return self.mon_begin(hw); }
                return xip_abi::RET_CONTINUE as i32;
            }
            if !call.is_host(HOST_START) { return xip_abi::RET_CONTINUE as i32; } // NETWORK, WAKE: not used
            let d = call.data;
            if d.starts_with(b"now") { self.mode = MODE_NOW; return self.now_start(hw, d); }
            if d.starts_with(b"mon") { self.mode = MODE_MON; return self.mon_start(hw, d); }
            if d.starts_with(b"async") {
                self.mode = MODE_ASYNC;
                let alarm = d.starts_with(b"async-alarm");
                let yielding = d.starts_with(b"async-yield");
                let pause = d.starts_with(b"async-pause");
                let context = d.starts_with(b"async-context");
                return self.task.start(hw, move |ctx| async move {
                    if context {
                        // A raw SDK component call between global allocations.
                        // No wrapper may replace the platform's native TLS context.
                        let mut before = Vec::<u8>::new();
                        if before.try_reserve_exact(32).is_err() { return ERR_NOMEM; }
                        before.extend_from_slice(b"before native call");
                        let mut digest = [0u8; 32];
                        let mut len = 0usize;
                        let rc = match ctx.with_hw(|h| {
                            let Some(psa) = xip_abi::psa::PsaTable::from_host(*h) else { return xip_abi::ERR_UNSUPPORTED; };
                            unsafe { (psa.psa_hash_compute)(xip_abi::psa::ALG_SHA_256, b"abc".as_ptr(), 3,
                                digest.as_mut_ptr(), digest.len(), &mut len) }
                        }) { Ok(rc) => rc, Err(e) => return e };
                        if rc != 0 { return rc; }
                        if len != 32 || digest != [0xba,0x78,0x16,0xbf,0x8f,0x01,0xcf,0xea,
                            0x41,0x41,0x40,0xde,0x5d,0xae,0x22,0x23,0xb0,0x03,0x61,0xa3,
                            0x96,0x17,0x7a,0x9c,0xb4,0x10,0xff,0x61,0xf2,0x00,0x15,0xad] { return ERR_ARG; }
                        if let Err(e) = ctx.sleep(1).await { return e; }
                        let mut after = Vec::<u8>::new();
                        if after.try_reserve_exact(32).is_err() { return ERR_NOMEM; }
                        after.extend_from_slice(&before);
                        if after != b"before native call" { return ERR_ARG; }
                        return xip_abi::RET_DONE as i32;
                    }
                    if yielding {
                        if let Err(e) = ctx.sleep(0).await { return e; }
                        let mut yielded = 0;
                        core::future::poll_fn(|cx| {
                            if yielded == 2 { core::task::Poll::Ready(()) } else {
                                yielded += 1; cx.waker().wake_by_ref(); core::task::Poll::Pending
                            }
                        }).await;
                        return xip_abi::RET_DONE as i32;
                    }
                    if alarm {
                        if let Err(e) = ctx.sleep(30).await { return e; }
                        if let Err(e) = ctx.with_hw(|h| h.event(C_TEST, EV_SUMMARY, b"async alarm ok")) { return e; }
                        return xip_abi::RET_DONE as i32;
                    }
                    // Device bench: leave time to burst requests over UART while
                    // the task is pending, then exercise queue refusal/recovery.
                    if pause { if let Err(e) = ctx.sleep(1000).await { return e; } }
                    let mut conn = 0u32;
                    let mut pending = Vec::<u8>::new();
                    let mut sent = 0usize;
                    loop {
                        let mut c = ctx.next_call().await;
                        if c.component == C_TEST && c.method == 0 && c.flags & xip_abi::CALL_LINK == 0 {
                            let handle = c.id as u32;
                            if c.value as u32 == xip_abi::SOCKET_EVENT_OPEN {
                                if conn == 0 { conn = handle; } else { let _ = ctx.with_hw(|h| h.socket_close(handle)); }
                                continue;
                            }
                            if handle != conn { continue; }
                            if c.value as u32 == xip_abi::SOCKET_EVENT_CLOSED || c.value as u32 == xip_abi::SOCKET_EVENT_EOF {
                                let _ = ctx.with_hw(|h| h.socket_close(handle));
                                conn = 0; pending.clear(); sent = 0;
                                continue;
                            }
                            if c.value as u32 != xip_abi::SOCKET_EVENT_READABLE && c.value as u32 != xip_abi::SOCKET_EVENT_WRITABLE { continue; }
                            loop {
                                if pending.is_empty() {
                                    let mut buf = [0u8; 256];
                                    if pending.try_reserve(256).is_err() { return ERR_NOMEM; }
                                    let n = match ctx.with_hw(|h| h.socket_read(handle, &mut buf)) { Ok(n) => n, Err(e) => return e };
                                    if n == ERR_AGAIN { break; }
                                    if n <= 0 {
                                        let _ = ctx.with_hw(|h| h.socket_close(handle)); conn = 0; break;
                                    }
                                    pending.extend_from_slice(&buf[..n as usize]); sent = 0;
                                }
                                let n = match ctx.with_hw(|h| h.socket_write(handle, &pending[sent..])) { Ok(n) => n, Err(e) => return e };
                                if n == ERR_AGAIN { break; }
                                if n <= 0 {
                                    let _ = ctx.with_hw(|h| h.socket_close(handle)); conn = 0; pending.clear(); sent = 0; break;
                                }
                                sent += n as usize;
                                if sent == pending.len() { pending.clear(); sent = 0; break; }
                            }
                            continue;
                        }
                        // Deliberately cross a handler boundary with the owned payload.
                        if let Err(e) = ctx.sleep(5).await { return e; }
                        if c.flags & xip_abi::CALL_LINK != 0 && c.flags & xip_abi::CALL_LINK_EVENT == 0 {
                            if let Some(packet) = c.packet.as_mut() {
                                let _ = packet.send(c.from, 0, 0, c.id);
                            }
                        } else if c.component == C_TEST && c.method == M_ECHO {
                            let v = match c.view() { Ok(v) => v, Err(e) => return e };
                            let wants_response = v.wants_response();
                            let text = cbor::get_text(v.data, 1).ok().flatten().unwrap_or(&[]);
                            if let Err(e) = ctx.with_hw(|h| { h.event(C_TEST, EV_ECHO, text); }) { return e; }
                            if wants_response {
                                if let Some(packet) = c.packet.as_mut() {
                                    // Reply with this same CBOR packet even when all
                                    // owned packet slots are occupied by queued calls.
                                    let _ = packet.send(c.from, c.component, c.method, c.id);
                                } else {
                                    let v = match c.view() { Ok(v) => v, Err(e) => return e };
                                    if let Err(e) = ctx.with_hw(|h| h.reply(&v, v.data)) { return e; }
                                }
                            }
                        }
                    }
                });
            }
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
            M_NOW_TICK => {
                self.now_tick(hw);
                xip_abi::RET_CONTINUE as i32
            }
            M_NOW_GUARD => self.now_end(hw),
            M_NOW_WAIT => {
                self.wait_connected(hw, 500);
                xip_abi::RET_CONTINUE as i32
            }
            M_MON_GUARD => self.mon_end(hw),
            M_MON_TICK => {
                let mut l = Line::new("mon: frames ");
                l.int(self.mon_frames as i64);
                hw.log(l.bytes());
                xip_abi::RET_CONTINUE as i32
            }
            M_NOW_TXDONE => {
                if call.flags & CALL_LINK != 0 && call.flags & CALL_LINK_EVENT != 0 && event_kind(call.flags) == LINK_EVENT_TX_DONE {
                    if call.value == 0 { self.now_txok += 1; } else { self.now_txerr += 1; }
                }
                xip_abi::RET_CONTINUE as i32
            }
            M_NOW_FRAME => {
                self.now_frame(hw, call);
                xip_abi::RET_CONTINUE as i32
            }
            M_MON_FRAME => {
                self.mon_frame(hw, call);
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
