//! The radio as an event-driven state machine. The host calls the module for a start request, a method request, the
//! radio's interrupt line (`M_IRQ`) and its own alarm (`M_TIMER`); every call does a bounded amount of SPI work and
//! returns. Waiting (the chip's settling times, a transmission, a CAD window) is an alarm or the interrupt line,
//! never a sleep, so between calls the host is free to sleep.
//!
//! Phases: `IDLE` (nothing initialised) → `INIT` (power, reset, SPI) → `READY` (board up, radio idle) →
//! `SETUP` (configuring) → `RX` / `CAD` / `CADRX` / `FSKRX` (the persistent receive modes) and `TX` (a
//! transmission; the receive mode comes back afterwards). A request that arrives while the machine is in `INIT`,
//! `SETUP` or `TX` waits in a one-deep queue; a second one gets `ERR_BUSY`.

use crate::command::*;
use crate::config::{LoraConfig, CHIP_SX126X};
use crate::fsk::*;
use crate::hal::{event_i32, Hal, Next, SpiSetup, MAX_PACKET};
use crate::sx126x::*;
use crate::sx127x::*;

/// The component number of the module: its slot tag, as in DMesh.
pub const COMPONENT: i32 = 43;
/// The radio's interrupt line fired.
pub const M_IRQ: i32 = 20;
/// The module's alarm fired.
pub const M_TIMER: i32 = 21;

/// Result of a start request that cannot be parsed (the module ends).
pub const ERR_REQUEST: i32 = -1;
const ERR_BUSY: i32 = -5;

// phases
const IDLE: u8 = 0;
const INIT: u8 = 1;
const READY: u8 = 2;
const SETUP: u8 = 3;
const RX: u8 = 4;
const CAD: u8 = 5;
const CADRX: u8 = 6;
const FSKRX: u8 = 7;
const TX: u8 = 8;
const CAD_WAIT: u8 = 9;   // the pause between two CAD windows

// persistent modes
const MODE_NONE: u8 = 0;
const MODE_RX: u8 = 1;
const MODE_CAD: u8 = 2;
const MODE_FSK: u8 = 3;

// what a finished setup leads to
const AFTER_MODE: u8 = 0;    // enter `mode`, announce it (event 1)
const AFTER_QUIET: u8 = 1;   // enter `mode` again without announcing (after a transmission or a reconfigure)
const AFTER_TX: u8 = 2;      // start the LoRa transmission
const AFTER_FSK_TX: u8 = 3;  // start the FSK transmission
const AFTER_RECONF: u8 = 4;  // event 4, then `mode` again (or standby)

// kinds of setup
const KIND_LORA: u8 = 0;
const KIND_LORA_AFTER_RESET: u8 = 1;   // SX126x: hard reset first, then the LoRa setup
const KIND_FSK: u8 = 2;

const TX_TIMEOUT_MS: u32 = 6000;
const FSK_TX_TIMEOUT_MS: u32 = 1000;

/// The module's state; it lives in the state block of the host, which is zeroed at the start: every field is valid
/// as all zeros (`cfg` is set to its defaults at the first call).
pub struct Radio {
    phase: u8,
    step: u8,
    mode: u8,
    resume: u8,
    kind: u8,
    after: u8,
    cfg_set: bool,
    irq_attached: bool,
    timer: i32, // alarm id + 1, 0 = none
    tx_fsk: bool,
    in_irq: bool,
    tx_stage: u8,
    pend_op: u8,
    pend_len: u8,
    cur_len: u8,
    cad_windows: u32,
    rx_packets: u32,
    tx_packets: u32,
    cfg: LoraConfig,
    pend: [u8; MAX_PAYLOAD],
    cur: [u8; MAX_PAYLOAD],
}

/// What a call answers: the value a reply carries, if the call was a request.
pub type Answer = Option<i32>;

impl Radio {
    fn ensure_cfg(&mut self) {
        if !self.cfg_set {
            self.cfg = LoraConfig::DEFAULT;
            self.cfg_set = true;
        }
    }

    fn is_126(&self) -> bool {
        self.cfg.chip == CHIP_SX126X
    }

    fn arm<H: Hal>(&mut self, h: &mut H, ms: u32) {
        self.disarm(h);
        let id = h.alarm(ms.max(1));
        if id >= 0 { self.timer = id + 1; }
    }

    fn disarm<H: Hal>(&mut self, h: &mut H) {
        if self.timer != 0 {
            h.cancel(self.timer - 1);
            self.timer = 0;
        }
    }

    fn irq<H: Hal>(&mut self, h: &mut H, on: bool) {
        // the line is claimed the first time it is needed (a probe never touches it)
        if on && self.cfg.irq_pin >= 0 && !self.irq_attached {
            self.irq_attached = h.irq_attach(self.cfg.irq_pin as i32) == 0;
        }
        if self.cfg.irq_pin >= 0 && self.irq_attached {
            let _ = h.irq_enable(self.cfg.irq_pin as i32, on);
        }
    }

    // ---- entry point ----

    /// One call of the host. Returns the handler result (0 keep running, negative: the module ends) and the answer
    /// of a request.
    pub fn handle<H: Hal>(&mut self, h: &mut H, component: i32, method: i32, data: &[u8]) -> (i32, Answer) {
        self.ensure_cfg();
        if component == 0 {
            return match method {
                1 => (self.start(h, data), None),
                2 => {
                    self.shutdown(h);
                    (0, None)
                }
                _ => (0, None),   // NETWORK, WAKE: nothing to do
            };
        }
        if component != COMPONENT { return (0, None); }
        match method {
            M_IRQ => self.on_irq(h),
            M_TIMER => {
                self.timer = 0;
                self.on_timer(h);
            }
            1..=9 => return (0, Some(self.request(h, method as u8, data))),   // OP_IDLE is for the start request only
            _ => return (0, Some(-2)),
        }
        (0, None)
    }

    fn start<H: Hal>(&mut self, h: &mut H, request: &[u8]) -> i32 {
        let mut payload = [0u8; MAX_PAYLOAD];
        match parse_start(request, &mut self.cfg, &mut payload) {
            Ok((op, n)) => {
                let rc = self.submit(h, op, &payload[..n]);
                if rc < 0 && rc != ERR_BUSY { h.log(b"mod_lora: start request failed"); }
                0
            }
            Err(_) => {
                h.log(b"mod_lora: bad start request");
                ERR_REQUEST
            }
        }
    }

    fn request<H: Hal>(&mut self, h: &mut H, op: u8, data: &[u8]) -> i32 {
        let mut payload = [0u8; MAX_PAYLOAD];
        let n = match apply_fields(data, &mut self.cfg, &mut payload) {
            Ok(n) => n,
            Err(_) => {
                h.log(b"mod_lora: bad request");
                return -2;
            }
        };
        self.submit(h, op, &payload[..n])
    }

    fn shutdown<H: Hal>(&mut self, h: &mut H) {
        self.disarm(h);
        self.irq(h, false);
        self.power_down(h);
    }

    fn power_down<H: Hal>(&mut self, h: &mut H) {
        if self.cfg.board_power_pin >= 0 {
            let off = if self.cfg.board_power_level == 0 { 1 } else { 0 };
            let _ = h.gpio_write(self.cfg.board_power_pin, off);
        }
    }

    // ---- operations ----

    /// Run `op` now, or queue it behind the work in progress (and bring the board up first if need be).
    fn submit<H: Hal>(&mut self, h: &mut H, op: u8, payload: &[u8]) -> i32 {
        if op == OP_IDLE { return 0; }
        if op == OP_STATS {
            let mut stats = [0u8; 8];
            stats[..4].copy_from_slice(&self.rx_packets.to_le_bytes());
            stats[4..].copy_from_slice(&self.tx_packets.to_le_bytes());
            h.event(5, &stats);
            return 0;
        }
        if op == OP_STOP && self.phase == IDLE {
            self.power_down(h);
            return 0;
        }
        if self.phase == IDLE || matches!(self.phase, INIT | SETUP | TX) {
            if self.pend_op != 0 { return ERR_BUSY; }
            self.pend_op = op;
            self.pend_len = payload.len() as u8;
            self.pend[..payload.len()].copy_from_slice(payload);
            if self.phase == IDLE { self.begin_init(h); }
            return 0;
        }
        self.run_op(h, op, payload)
    }

    /// The board is up and the machine is not mid-sequence.
    fn run_op<H: Hal>(&mut self, h: &mut H, op: u8, payload: &[u8]) -> i32 {
        match op {
            OP_STOP => {
                self.disarm(h);
                self.irq(h, false);
                self.mode = MODE_NONE;
                self.resume = MODE_NONE;
                self.power_down(h);
                h.spi_close();
                self.phase = IDLE;
                h.log(b"mod_lora stopped");
                h.event(2, &[]);
                0
            }
            OP_PROBE => {
                event_i32(h, 31, self.cfg.chip as i32);
                let rc = if self.is_126() { probe_sx126x(h) } else { probe_sx127x(h) };
                h.log(if rc == 0 { b"mod_lora probe ok" } else { b"mod_lora probe failed" });
                if rc != 0 {
                    event_i32(h, 6, rc);
                    // what the chip answered, to tell a wiring or reset fault from a wrong chip
                    let mut d = [0u8; 6];
                    for (i, r) in [REG_VERSION, 0x01u8, 0x06, 0x07, 0x08, 0x39].iter().enumerate() { d[i] = sx127x_read(h, *r).unwrap_or(0xee); }
                    h.event(33, &d);
                }
                rc
            }
            OP_PROBE127 => probe_sx127x(h),
            OP_PROBE126 => probe_sx126x(h),
            OP_RX => {
                self.mode = if self.cfg.cad_rx != 0 { MODE_CAD } else { MODE_RX };
                self.begin_setup(h, self.lora_kind(false), AFTER_MODE);
                0
            }
            OP_RECONFIGURE => {
                if self.mode == MODE_FSK {
                    self.begin_setup(h, KIND_FSK, AFTER_RECONF);
                } else {
                    self.begin_setup(h, self.lora_kind(false), AFTER_RECONF);
                }
                0
            }
            OP_TX | OP_FSK => self.begin_tx(h, op, payload),
            _ => -2,
        }
    }

    fn lora_kind(&self, tx: bool) -> u8 {
        if tx && self.is_126() { KIND_LORA_AFTER_RESET } else { KIND_LORA }
    }

    /// A transmission: LoRa (`tx`, or in a LoRa session) or FSK (`fsk` with data, or `tx` in an FSK session).
    fn begin_tx<H: Hal>(&mut self, h: &mut H, op: u8, payload: &[u8]) -> i32 {
        let fsk = op == OP_FSK || self.mode == MODE_FSK;
        if op == OP_FSK && payload.is_empty() {
            // `fsk` without data: stay in FSK receive
            self.mode = MODE_FSK;
            self.begin_setup(h, KIND_FSK, AFTER_MODE);
            h.log(b"mod_lora FSK selected");
            return 0;
        }
        if payload.is_empty() || payload.len() > MAX_PACKET { return -1; }
        self.cur[..payload.len()].copy_from_slice(payload);
        self.cur_len = payload.len() as u8;
        self.tx_fsk = fsk;
        self.resume = self.mode;
        self.disarm(h);
        self.irq(h, false);
        if fsk {
            event_i32(h, 20, payload.len() as i32);
            self.begin_setup(h, KIND_FSK, AFTER_FSK_TX);
        } else if self.is_126() {
            // The SX126x goes through a reset and the complete setup before every transmission (calibrated PA/RF path).
            self.begin_setup(h, KIND_LORA_AFTER_RESET, AFTER_TX);
        } else if self.phase == READY {
            // A one-shot after `stop`: apply the complete configuration before touching the FIFO.
            self.begin_setup(h, KIND_LORA, AFTER_TX);
        } else {
            self.start_lora_tx(h);
        }
        0
    }

    // ---- board bring-up ----

    fn begin_init<H: Hal>(&mut self, h: &mut H) {
        self.phase = INIT;
        self.step = 0;
        self.init_step(h);
    }

    fn init_step<H: Hal>(&mut self, h: &mut H) {
        let mut step = self.step;
        loop {
            match step {
                0 => {
                    if self.cfg.board_power_pin >= 0 {
                        let _ = h.gpio_output(self.cfg.board_power_pin, (self.cfg.board_power_level != 0) as i32);
                        // An externally powered radio/TCXO needs a moment before its reset line is released.
                        self.step = 1;
                        return self.arm(h, 2);
                    }
                    step = 1;
                }
                1 => {
                    if self.cfg.reset_pin >= 0 {
                        let _ = h.gpio_output(self.cfg.reset_pin as i32, 0);
                        self.step = 2;
                        return self.arm(h, 10);
                    }
                    step = 3;
                }
                2 => {
                    let _ = h.gpio_write(self.cfg.reset_pin as i32, 1);
                    self.step = 3;
                    return self.arm(h, 20);
                }
                _ => {
                    let rc = h.spi_open(&SpiSetup {
                        bus: self.cfg.spi_bus,
                        sck: self.cfg.sck_pin as i32,
                        miso: self.cfg.miso_pin as i32,
                        mosi: self.cfg.mosi_pin as i32,
                        cs: self.cfg.cs_pin as i32,
                        hz: 1_000_000,
                        mode: 0,
                        ready_pin: self.cfg.busy_pin as i32,
                    });
                    return self.init_done(h, rc);
                }
            }
        }
    }

    fn init_done<H: Hal>(&mut self, h: &mut H, rc: i32) {
        if rc < 0 {
            h.log(b"mod_lora: board init failed");
            event_i32(h, 6, rc);
            self.phase = IDLE;
            self.pend_op = 0;
            return;
        }
        self.phase = READY;
        self.drain(h);
    }

    // ---- configuration sequences ----

    fn begin_setup<H: Hal>(&mut self, h: &mut H, kind: u8, after: u8) {
        self.disarm(h);
        self.irq(h, false);
        self.phase = SETUP;
        self.kind = kind;
        self.after = after;
        self.step = if kind == KIND_LORA && self.is_126() { 10 } else { 0 };
        self.setup_step(h);
    }

    fn setup_step<H: Hal>(&mut self, h: &mut H) {
        let next = if self.kind == KIND_FSK {
            if self.is_126() { sx126x_fsk_setup_step(h, &self.cfg, self.step) } else { sx127x_fsk_setup_step(h, &self.cfg, self.step) }
        } else if !self.is_126() {
            Next::Done(sx127x_configure_rx(h, &self.cfg))
        } else {
            self.lora_126_step(h)
        };
        match next {
            Next::Wait(ms, step) => {
                self.step = step;
                self.arm(h, ms);
            }
            Next::Done(rc) => self.setup_done(h, rc),
        }
    }

    /// SX126x LoRa setup: optional hard reset (steps 0-1), first half, TCXO settling, second half.
    fn lora_126_step<H: Hal>(&mut self, h: &mut H) -> Next {
        let mut step = self.step;
        loop {
            match step {
                0 => {
                    if self.cfg.reset_pin < 0 { step = 10; continue; }
                    let _ = h.gpio_write(self.cfg.reset_pin as i32, 0);
                    return Next::Wait(30, 1);
                }
                1 => {
                    let _ = h.gpio_write(self.cfg.reset_pin as i32, 1);
                    return Next::Wait(20, 10);
                }
                10 => {
                    let rc = sx126x_configure_a(h, &self.cfg);
                    if rc != 0 { return Next::Done(rc); }
                    if self.cfg.sx1262_tcxo_mv > 0 { return Next::Wait(5, 11); }
                    step = 11;
                }
                _ => return Next::Done(sx126x_configure_b(h, &self.cfg)),
            }
        }
    }

    fn setup_done<H: Hal>(&mut self, h: &mut H, rc: i32) {
        if rc != 0 {
            h.log(b"mod_lora: radio setup failed");
            event_i32(h, 6, rc);
            self.mode = MODE_NONE;
            self.resume = MODE_NONE;
            self.phase = READY;
            self.pend_op = 0;
            return;
        }
        match self.after {
            AFTER_TX => self.start_lora_tx(h),
            AFTER_FSK_TX => self.start_fsk_tx(h),
            AFTER_RECONF => {
                h.event(4, &[]);
                self.enter_mode(h, false);
            }
            AFTER_QUIET => self.enter_mode(h, false),
            _ => self.enter_mode(h, true),
        }
    }

    /// Start (or restart) the persistent mode `self.mode` on a configured radio.
    fn enter_mode<H: Hal>(&mut self, h: &mut H, announce: bool) {
        match self.mode {
            MODE_RX => {
                self.phase = RX;
                self.irq(h, true);
                if announce {
                    h.log(if self.is_126() { b"mod_lora sx126x rx started" } else { b"mod_lora sx127x rx started" });
                    h.event(1, &[]);
                }
                self.check_pending_irq(h);
            }
            MODE_CAD => {
                if announce {
                    h.log(b"mod_lora cad rx started");
                    h.event(1, &[]);
                }
                self.cad_begin(h);
            }
            MODE_FSK => {
                let rc = if self.is_126() { sx126x_fsk_rx_enter(h) } else { sx127x_fsk_rx_enter(h) };
                if rc != 0 {
                    event_i32(h, 6, rc);
                    h.log(b"mod_lora fsk rx setup failed");
                    self.mode = MODE_NONE;
                    self.phase = READY;
                } else {
                    self.phase = FSKRX;
                    self.irq(h, true);
                    if announce {
                        h.log(b"mod_lora fsk rx started");
                        h.event(1, &[]);
                    }
                    self.check_pending_irq(h);
                }
            }
            _ => {
                if self.is_126() { let _ = sx126x_command(h, SX126X_CMD_SET_STANDBY, &[0]); } else { sx127x_standby(h); }
                self.phase = READY;
            }
        }
        self.drain(h);
    }

    /// The interrupt line may already be high (a packet arrived while configuring): the edge is gone, the level is not.
    fn check_pending_irq<H: Hal>(&mut self, h: &mut H) {
        // (not from inside the interrupt handling: it re-reads the line itself and must not recurse)
        if !self.in_irq && self.cfg.irq_pin >= 0 && h.gpio_read(self.cfg.irq_pin as i32) > 0 {
            self.on_irq(h);
        }
    }

    /// Run the queued request, if the machine can take it now.
    fn drain<H: Hal>(&mut self, h: &mut H) {
        if self.pend_op == 0 || matches!(self.phase, IDLE | INIT | SETUP | TX) { return; }
        let op = self.pend_op;
        let len = self.pend_len as usize;
        let mut payload = [0u8; MAX_PAYLOAD];
        payload[..len].copy_from_slice(&self.pend[..len]);
        self.pend_op = 0;
        let _ = self.run_op(h, op, &payload[..len]);
    }

    // ---- transmission ----

    fn start_lora_tx<H: Hal>(&mut self, h: &mut H) {
        let len = self.cur_len as usize;
        let mut packet = [0u8; MAX_PAYLOAD];
        packet[..len].copy_from_slice(&self.cur[..len]);
        let rc = if self.is_126() { sx126x_tx_begin(h, &self.cfg, &packet[..len]) } else { sx127x_tx_begin(h, &packet[..len]) };
        if rc != 0 { return self.tx_finished(h, rc); }
        self.phase = TX;
        self.tx_stage = 2;
        self.irq(h, true);
        self.arm(h, TX_TIMEOUT_MS);
        self.check_pending_irq(h);
    }

    fn start_fsk_tx<H: Hal>(&mut self, h: &mut H) {
        let len = self.cur_len as usize;
        let mut packet = [0u8; MAX_PAYLOAD];
        packet[..len].copy_from_slice(&self.cur[..len]);
        if self.is_126() {
            let rc = sx126x_fsk_tx_begin(h, &packet[..len]);
            if rc != 0 { return self.tx_finished(h, rc); }
            self.phase = TX;
            self.tx_stage = 2;
            self.irq(h, true);
            self.arm(h, FSK_TX_TIMEOUT_MS);
        } else {
            let rc = sx127x_fsk_send_begin(h, &packet[..len]);
            if rc != 0 { return self.tx_finished(h, rc); }
            self.phase = TX;
            self.tx_stage = 1;   // FSTx: one millisecond, then the transmission
            self.arm(h, 1);
        }
    }

    /// The transmission ended with `rc` (0 sent): report, then go back to the receive mode that was running.
    fn tx_finished<H: Hal>(&mut self, h: &mut H, rc: i32) {
        self.disarm(h);
        self.irq(h, false);
        let len = self.cur_len as usize;
        if rc == 0 {
            self.tx_packets = self.tx_packets.saturating_add(1);
            let mut packet = [0u8; MAX_PAYLOAD];
            packet[..len].copy_from_slice(&self.cur[..len]);
            h.event(3, &packet[..len]);
        } else {
            h.log(b"mod_lora tx failed");
            event_i32(h, 6, rc);
        }
        self.mode = self.resume;
        // The chip comes back through its complete setup: an SX126x after a transmission, an SX127x FSK transmission
        // (another register bank), or a LoRa transmission interrupting a session.
        if self.mode == MODE_NONE {
            self.phase = READY;   // a transmission ends in standby on both chips
            return self.drain(h);
        }
        let kind = if self.mode == MODE_FSK { KIND_FSK } else { KIND_LORA };
        self.begin_setup(h, kind, AFTER_QUIET);
    }

    fn on_tx_event<H: Hal>(&mut self, h: &mut H, timed_out: bool) {
        if self.tx_fsk {
            if self.is_126() {
                match sx126x_tx_poll(h) {
                    TxPoll::Done => self.tx_finished(h, 0),
                    TxPoll::Failed(rc) => self.tx_finished(h, rc),
                    TxPoll::Pending if timed_out => {
                        let rc = sx126x_tx_abort(h);
                        self.tx_finished(h, rc)
                    }
                    TxPoll::Pending => {}
                }
            } else if sx127x_fsk_sent(h) {
                self.tx_finished(h, 0);
            } else if timed_out {
                self.tx_finished(h, -1000);
            }
        } else if self.is_126() {
            match sx126x_tx_poll(h) {
                TxPoll::Done => self.tx_finished(h, 0),
                TxPoll::Failed(rc) => self.tx_finished(h, rc),
                TxPoll::Pending if timed_out => {
                    let rc = sx126x_tx_abort(h);
                    self.tx_finished(h, rc)
                }
                TxPoll::Pending => {}
            }
        } else if sx127x_tx_done(h) {
            self.tx_finished(h, 0);
        } else if timed_out {
            self.tx_finished(h, -4);
        }
    }

    // ---- CAD ----

    /// The next CAD window after `cad_period` ms of idle (the chip sleeps, the CPU too).
    fn cad_later<H: Hal>(&mut self, h: &mut H) {
        self.irq(h, false);
        self.phase = CAD_WAIT;
        self.arm(h, self.cfg.cad_period_ms);
    }

    fn cad_begin<H: Hal>(&mut self, h: &mut H) {
        let rc = if self.is_126() { sx126x_cad_begin(h) } else { sx127x_cad_begin(h) };
        if rc != 0 {
            event_i32(h, 9, rc);
            self.phase = READY;
            self.mode = MODE_NONE;
            return;
        }
        self.phase = CAD;
        self.irq(h, true);
        let wait = if self.is_126() { self.cfg.cad_interval_ms.max(5).min(1000) }
                   else if self.cfg.cad_interval_ms == 0 { 5 } else { self.cfg.cad_interval_ms };
        self.arm(h, wait);
        self.check_pending_irq(h);
    }

    /// A CAD window ended (interrupt or timeout): quiet → the next window; activity → a bounded receive window.
    fn cad_end<H: Hal>(&mut self, h: &mut H) {
        self.disarm(h);
        let (end, diag) = if self.is_126() { sx126x_cad_end(h) } else { (sx127x_cad_end(h), 0) };
        self.cad_windows = self.cad_windows.saturating_add(1);
        match end {
            CadEnd::Detected => {
                let window = if self.is_126() { self.cfg.cad_rx_ms.max(1).min(4000) }
                             else if self.cfg.cad_rx_ms == 0 { 1000 } else { self.cfg.cad_rx_ms };
                if !self.is_126() && sx127x_rx_single(h) != 0 {
                    event_i32(h, 9, -3);
                    return self.cad_begin(h);
                }
                self.phase = CADRX;
                self.arm(h, window);
                self.check_pending_irq(h);
            }
            other => {
                // 7: quiet (every 20th window), 11 or 32+status: the window never finished, 9: SPI error
                let (id, value) = match other {
                    CadEnd::Quiet if self.cad_windows % 20 == 0 => (7u16, 1),
                    CadEnd::Quiet => (0, 0),
                    CadEnd::NotDone if diag >= 0x100 => (32 + (diag & 0xff) as u16, diag),
                    CadEnd::NotDone if diag != 0 => (11, diag),
                    CadEnd::NotDone => (0, 0),
                    CadEnd::Error(e) => (9, e),
                    CadEnd::Detected => (0, 0),
                };
                if id != 0 { event_i32(h, id, value); }
                self.cad_later(h);
            }
        }
    }

    fn cadrx_end<H: Hal>(&mut self, h: &mut H) {
        self.disarm(h);
        let got = self.emit_packet(h) == 0;
        if !got {
            if self.is_126() { let _ = sx126x_command(h, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]); } else { sx127x_standby(h); }
            event_i32(h, 8, 2);
        }
        self.cad_later(h);
    }

    // ---- events from the host ----

    fn emit_packet<H: Hal>(&mut self, h: &mut H) -> i32 {
        let rc = if self.phase == FSKRX {
            if self.is_126() { sx126x_fsk_emit_packet(h) } else { sx127x_fsk_emit_packet(h) }
        } else if self.is_126() {
            sx126x_emit_packet(h)
        } else {
            sx127x_emit_packet(h)
        };
        if rc == 0 { self.rx_packets = self.rx_packets.saturating_add(1); }
        rc
    }

    fn on_irq<H: Hal>(&mut self, h: &mut H) {
        self.in_irq = true;
        self.on_irq_inner(h);
        self.in_irq = false;
    }

    fn on_irq_inner<H: Hal>(&mut self, h: &mut H) {
        match self.phase {
            RX => {
                // The line stays high while the chip has more to say: read until it drops (a few times at most).
                for _ in 0..4 {
                    let _ = self.emit_packet(h);
                    if self.cfg.irq_pin < 0 || h.gpio_read(self.cfg.irq_pin as i32) <= 0 { break; }
                }
            }
            FSKRX => {
                for _ in 0..4 {
                    let rc = self.emit_packet(h);
                    if rc == 0 {
                        h.event(22, &[]);
                        if self.is_126() { let _ = sx126x_fsk_rearm_rx(h); }
                    } else if rc >= 0x1000 {
                        if self.is_126() { let _ = sx126x_fsk_rearm_rx(h); }
                    } else if rc < 0 {
                        event_i32(h, 23, rc);
                    }
                    if self.cfg.irq_pin < 0 || h.gpio_read(self.cfg.irq_pin as i32) <= 0 { break; }
                }
            }
            CAD => self.cad_end(h),
            CADRX => self.cadrx_end(h),
            TX => self.on_tx_event(h, false),
            _ => {}
        }
    }

    fn on_timer<H: Hal>(&mut self, h: &mut H) {
        match self.phase {
            INIT => self.init_step(h),
            SETUP => self.setup_step(h),
            CAD => self.cad_end(h),
            CADRX => self.cadrx_end(h),
            CAD_WAIT => self.cad_begin(h),
            TX => {
                if self.tx_stage == 1 {
                    // FSK on the SX127x: FSTx has settled, start the transmission
                    let rc = sx127x_fsk_send_go(h);
                    if rc != 0 { return self.tx_finished(h, rc); }
                    self.tx_stage = 2;
                    self.irq(h, true);
                    self.arm(h, FSK_TX_TIMEOUT_MS);
                } else {
                    self.on_tx_event(h, true);
                }
            }
            _ => {}
        }
    }
}
