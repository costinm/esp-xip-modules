//! The radio machine against a scripted fake board: SX127x registers and SX126x commands, a clock of alarms that the
//! test fires by hand, and the interrupt line.

use crate::command::*;
use crate::config::LoraConfig;
use crate::hal::{Hal, SpiSetup};
use crate::machine::*;
use crate::sx126x::*;
use crate::sx127x::*;
use std::collections::VecDeque;
use std::string::String;
use std::vec::Vec;

#[derive(Default)]
struct Fake {
    // SX127x
    regs: Vec<u8>,
    irq127: u8,
    irq127_fsk2: u8,
    rx_packet: Option<Vec<u8>>,
    never_tx_done: bool,
    // SX126x
    status: u8,
    irq126: u16,
    modes: Vec<u8>,
    ops: Vec<u8>,
    // board
    gpio: Vec<(i32, i32)>,
    irq_level: i32,
    irq_attached: Vec<i32>,
    irq_enabled: Vec<bool>,
    spi_opened: u32,
    spi_closed: u32,
    alarms: VecDeque<(i32, u32)>,
    next_alarm: i32,
    delays: Vec<u32>,
    // results
    packets: Vec<(Vec<u8>, i16, i8)>,
    logs: Vec<String>,
    events: Vec<(u16, Vec<u8>)>,
}

impl Fake {
    fn new() -> Self {
        let mut f = Fake::default();
        f.regs = std::vec![0u8; 256];
        f.regs[0x42] = 0x12;
        f.status = 0x22;
        f
    }

    fn op_mode(&self) -> u8 { self.regs[SX127X_REG_OP_MODE as usize] }
}

impl Hal for Fake {
    fn spi_open(&mut self, _s: &SpiSetup) -> i32 { self.spi_opened += 1; 0 }
    fn spi_close(&mut self) { self.spi_closed += 1; }

    fn spi(&mut self, tx: &[u8], rx: &mut [u8]) -> i32 {
        let op = tx[0];
        self.ops.push(op);
        // SX126x commands have opcodes the SX127x write form (reg | 0x80) never matches in these tests: the chip is
        // told apart by the test (`sx126x` flag in the status byte).
        if self.status == 0xff {
            return self.spi126(tx, rx);
        }
        if op & 0x80 != 0 {
            let reg = (op & 0x7f) as usize;
            if reg == SX127X_REG_FIFO as usize { return 0; }
            let v = tx[1];
            if reg == SX127X_REG_IRQ_FLAGS as usize { self.irq127 &= !v; self.irq_level = 0; return 0; }
            if reg == SX127X_REG_IRQ_FLAGS2 as usize { self.irq127_fsk2 &= !v; return 0; }
            if reg == SX127X_REG_OP_MODE as usize { self.modes.push(v); }
            self.regs[reg] = v;
        } else {
            let reg = op as usize;
            let v = if reg == SX127X_REG_IRQ_FLAGS as usize {
                if self.regs[SX127X_REG_OP_MODE as usize] & 7 == SX127X_MODE_TX && !self.never_tx_done { SX127X_IRQ_TX_DONE | self.irq127 } else { self.irq127 }
            } else if reg == SX127X_REG_IRQ_FLAGS2 as usize {
                if self.regs[SX127X_REG_OP_MODE as usize] & 7 == SX127X_MODE_TX && !self.never_tx_done { SX127X_FSK_IRQ2_PACKET_SENT | self.irq127_fsk2 } else { self.irq127_fsk2 }
            } else if reg == SX127X_REG_RX_BYTES as usize {
                self.rx_packet.as_ref().map(|p| p.len() as u8).unwrap_or(0)
            } else if reg == SX127X_REG_PKT_RSSI as usize { 157 - 60 }
            else if reg == SX127X_REG_FIFO as usize {
                if let Some(p) = &self.rx_packet { for (i, b) in p.iter().enumerate() { if i + 1 < rx.len() { rx[i + 1] = *b; } } }
                0
            } else { self.regs[reg] };
            if reg != SX127X_REG_FIFO as usize { rx[1] = v; }
        }
        0
    }

    fn gpio_output(&mut self, pin: i32, level: i32) -> i32 { self.gpio.push((pin, level)); 0 }
    fn gpio_write(&mut self, pin: i32, level: i32) -> i32 { self.gpio.push((pin, level)); 0 }
    fn gpio_read(&mut self, _pin: i32) -> i32 { self.irq_level }
    fn irq_attach(&mut self, pin: i32) -> i32 { self.irq_attached.push(pin); 0 }
    fn irq_enable(&mut self, _pin: i32, on: bool) -> i32 { self.irq_enabled.push(on); 0 }
    fn alarm(&mut self, ms: u32) -> i32 { self.next_alarm += 1; self.alarms.push_back((self.next_alarm, ms)); self.next_alarm }
    fn cancel(&mut self, id: i32) { self.alarms.retain(|a| a.0 != id); }
    fn packet(&mut self, data: &[u8], rssi: i16, snr: i8) -> i32 { self.packets.push((data.to_vec(), rssi, snr)); 0 }
    fn log(&mut self, text: &[u8]) { self.logs.push(String::from_utf8_lossy(text).into_owned()); }
    fn event(&mut self, id: u16, value: &[u8]) { self.events.push((id, value.to_vec())); }
}

impl Fake {
    fn spi126(&mut self, tx: &[u8], rx: &mut [u8]) -> i32 {
        match tx[0] {
            SX126X_GET_STATUS => rx[1] = 0x22,
            SX126X_CMD_GET_IRQ_STATUS => { rx[2] = (self.irq126 >> 8) as u8; rx[3] = self.irq126 as u8; }
            SX126X_CMD_CLEAR_IRQ_STATUS => { let m = u16::from_be_bytes([tx[1], tx[2]]); self.irq126 &= !m; self.irq_level = 0; }
            SX126X_CMD_GET_RX_BUFFER_STATUS => {
                rx[2] = self.rx_packet.as_ref().map(|p| p.len() as u8).unwrap_or(0);
                rx[3] = 0;
            }
            SX126X_CMD_READ_BUFFER => {
                if let Some(p) = &self.rx_packet { for (i, b) in p.iter().enumerate() { if 3 + i < rx.len() { rx[3 + i] = *b; } } }
            }
            SX126X_CMD_GET_PACKET_STATUS => { rx[2] = 120; rx[3] = 20; rx[4] = 118; }
            SX126X_CMD_SET_TX => { self.irq126 |= SX126X_IRQ_TX_DONE; }
            _ => {}
        }
        0
    }
}

/// A machine, a board and a clock.
struct Sim {
    radio: std::boxed::Box<Radio>,
    fake: Fake,
}

impl Sim {
    fn new() -> Self {
        // the state block of the host starts zeroed
        let radio = unsafe { std::boxed::Box::<Radio>::new_zeroed().assume_init() };
        Sim { radio, fake: Fake::new() }
    }

    fn sx126x() -> Self {
        let mut s = Sim::new();
        s.fake.status = 0xff;
        s
    }

    fn start(&mut self, request: &str) -> i32 {
        self.radio.handle(&mut self.fake, 0, 1, request.as_bytes()).0
    }

    fn call(&mut self, method: i32, fields: &[u8]) -> Option<i32> {
        self.radio.handle(&mut self.fake, COMPONENT, method, fields).1
    }

    /// Fire the oldest alarm.
    fn tick(&mut self) -> bool {
        let Some((_, ms)) = self.fake.alarms.pop_front() else { return false };
        self.fake.delays.push(ms);
        self.radio.handle(&mut self.fake, COMPONENT, M_TIMER, &[]);
        true
    }

    /// Fire the alarms of the bring-up and the setup, but not the safety timeout of a transmission (the longest one).
    fn settle_init(&mut self) {
        while self.fake.alarms.iter().any(|a| a.1 < 1000) {
            if !self.tick() { return; }
        }
    }

    /// Fire alarms until none is pending (bounded).
    fn settle(&mut self) {
        for _ in 0..50 { if !self.tick() { return; } }
    }

    fn irq(&mut self) {
        self.fake.irq_level = 1;   // the fake drops the line when the chip's flags are cleared
        self.radio.handle(&mut self.fake, COMPONENT, M_IRQ, &[]);
        self.fake.irq_level = 0;
    }

    fn events(&self, id: u16) -> usize { self.fake.events.iter().filter(|e| e.0 == id).count() }
    fn logged(&self, text: &str) -> bool { self.fake.logs.iter().any(|l| l.contains(text)) }
}

fn map(fields: &[(u64, i64)], data: Option<&[u8]>) -> Vec<u8> {
    use xip_abi::cbor::Writer;
    let mut buf = [0u8; 400];
    let mut w = Writer::new(&mut buf);
    w.map(fields.len() + data.is_some() as usize);
    for (k, v) in fields { w.uint(*k).int(*v); }
    if let Some(d) = data { w.uint(31).bytes(d); }
    w.finish().unwrap().to_vec()
}

// ---- requests ----

#[test]
fn start_requests_parse_in_both_forms() {
    let mut c = LoraConfig::DEFAULT;
    let mut p = [0u8; MAX_PAYLOAD];
    assert_eq!(parse_start(b"", &mut c, &mut p), Ok((OP_PROBE, 0)));
    assert_eq!(parse_start(b"rx freq=915000000 sf=9", &mut c, &mut p), Ok((OP_RX, 0)));
    assert_eq!((c.frequency_hz, c.spreading_factor), (915_000_000, 9));
    assert_eq!(parse_start(b"tx text=hello", &mut c, &mut p), Ok((OP_TX, 5)));
    assert_eq!(&p[..5], b"hello");
    assert_eq!(parse_start(b"tx data=0a0b", &mut c, &mut p), Ok((OP_TX, 2)));
    assert!(parse_start(b"bogus", &mut c, &mut p).is_err());
    assert!(parse_start(b"rx nosuch=1", &mut c, &mut p).is_err());
    let cbor = [0xa3, 0x00, 0x62, b't', b'x', 0x02, 0x1a, 0x35, 0xa4, 0xe4, 0x80, 0x1f, 0x41, 0x61, 0x00]; // {0:"tx",2:900000000,31:h'61'}
    // (the map above has 3 pairs but the last pair is incomplete: malformed)
    assert!(parse_start(&cbor[..cbor.len() - 1], &mut c, &mut p).is_err());
    let good = [0xa3, 0x00, 0x62, b't', b'x', 0x02, 0x1a, 0x35, 0xa4, 0xe4, 0x80, 0x18, 0x1f, 0x41, 0x61];
    let _ = good;
}

#[test]
fn request_fields_apply_to_the_config() {
    let mut c = LoraConfig::DEFAULT;
    let mut p = [0u8; MAX_PAYLOAD];
    let m = map(&[(2, 868_100_000), (4, 7)], Some(b"hi"));
    assert_eq!(apply_fields(&m, &mut c, &mut p), Ok(2));
    assert_eq!((c.frequency_hz, c.spreading_factor), (868_100_000, 7));
    assert_eq!(&p[..2], b"hi");
    assert_eq!(apply_fields(&map(&[(99, 1)], None), &mut c, &mut p), Err(-2));
    assert_eq!(apply_fields(&[0xff], &mut c, &mut p), Err(-1));
    assert_eq!(apply_fields(&[], &mut c, &mut p), Ok(0));
}

// ---- bring-up and probe ----

#[test]
fn the_board_comes_up_by_alarms_and_the_probe_runs_after() {
    let mut s = Sim::new();
    assert_eq!(s.start("probe"), 0);
    // reset low, 10 ms, reset high, 20 ms, then SPI: nothing has run synchronously beyond the first step
    assert_eq!(s.fake.spi_opened, 0);
    assert_eq!(s.fake.alarms.len(), 1);
    s.settle();
    assert_eq!(s.fake.delays, std::vec![10, 20]);
    assert_eq!(s.fake.gpio, std::vec![(14, 0), (14, 1)]);
    assert_eq!(s.fake.spi_opened, 1);
    assert!(s.logged("probe ok"));
    assert_eq!(s.events(31), 1);
}

#[test]
fn a_board_power_rail_is_switched_on_first() {
    let mut s = Sim::new();
    s.start("probe pwr=21 pwrlvl=1 rst=12");
    s.settle();
    assert_eq!(s.fake.gpio[0], (21, 1));
    assert_eq!(s.fake.delays, std::vec![2, 10, 20]);
}

#[test]
fn a_missing_radio_is_reported() {
    let mut s = Sim::new();
    s.fake.regs[0x42] = 0;
    s.start("probe");
    s.settle();
    assert!(s.logged("probe failed"));
    assert_eq!(s.events(6), 1);
}

#[test]
fn an_idle_start_does_nothing_until_the_first_request() {
    let mut s = Sim::new();
    assert_eq!(s.start("idle"), 0);
    assert!(s.fake.alarms.is_empty() && s.fake.spi_opened == 0 && s.fake.gpio.is_empty());
    s.call(1, &[]);
    s.settle();
    assert!(s.logged("probe ok"));
}

#[test]
fn a_bad_start_request_ends_the_module() {
    let mut s = Sim::new();
    assert_eq!(s.start("nonsense"), ERR_REQUEST);
}

// ---- receive ----

fn receiving() -> Sim {
    let mut s = Sim::new();
    s.start("rx");
    s.settle();
    s
}

#[test]
fn rx_configures_the_radio_and_waits_for_the_interrupt() {
    let s = receiving();
    assert_eq!(s.fake.op_mode(), SX127X_LONG_RANGE | SX127X_MODE_RX_CONTINUOUS);
    assert_eq!(s.fake.irq_attached, std::vec![26]);
    assert_eq!(s.fake.irq_enabled.last(), Some(&true));
    assert_eq!(s.events(1), 1);
    assert!(s.fake.alarms.is_empty(), "nothing is polling");
}

#[test]
fn a_packet_is_delivered_when_the_line_rises() {
    let mut s = receiving();
    s.fake.irq127 = SX127X_IRQ_RX_DONE;
    s.fake.rx_packet = Some(b"hello".to_vec());
    s.irq();
    assert_eq!(s.fake.packets, std::vec![(b"hello".to_vec(), -60, 0)]);
    s.call(9, &[]);
    let stats = s.fake.events.iter().find(|e| e.0 == 5).unwrap();
    assert_eq!(stats.1[..4], 1u32.to_le_bytes());
}

#[test]
fn a_crc_error_delivers_nothing() {
    let mut s = receiving();
    s.fake.irq127 = SX127X_IRQ_RX_DONE | SX127X_IRQ_CRC_ERROR;
    s.fake.rx_packet = Some(b"bad".to_vec());
    s.irq();
    assert!(s.fake.packets.is_empty());
}

// ---- transmit ----

#[test]
fn tx_during_rx_finishes_on_the_interrupt_and_receive_resumes() {
    let mut s = receiving();
    let m = map(&[], Some(b"ping"));
    assert_eq!(s.call(5, &m), Some(0));
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_TX);
    assert_eq!(s.fake.regs[SX127X_REG_DIO_MAPPING as usize], SX127X_DIO0_TX_DONE);
    assert_eq!(s.fake.alarms.len(), 1, "a safety timeout, not a polling loop");
    s.irq();
    assert_eq!(s.events(3), 1);
    assert_eq!(s.fake.op_mode(), SX127X_LONG_RANGE | SX127X_MODE_RX_CONTINUOUS, "receiving again");
    assert!(s.fake.alarms.is_empty());
    assert_eq!(s.events(1), 1, "the start of reception is not announced twice");
}

#[test]
fn a_transmission_that_never_ends_times_out_and_receive_resumes() {
    let mut s = receiving();
    s.fake.never_tx_done = true;
    s.call(5, &map(&[], Some(b"ping")));
    s.tick();
    assert_eq!(s.events(3), 0);
    assert_eq!(s.fake.events.iter().find(|e| e.0 == 6).map(|e| e.1.clone()), Some((-4i32).to_le_bytes().to_vec()));
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_RX_CONTINUOUS);
}

#[test]
fn a_one_shot_tx_without_a_session_configures_first() {
    let mut s = Sim::new();
    s.start("tx text=hello freq=915000000");
    s.settle_init();
    s.irq();
    assert_eq!(s.events(3), 1);
    assert!(s.fake.alarms.is_empty());
}

#[test]
fn a_request_queues_one_deep_while_the_board_comes_up() {
    let mut s = Sim::new();
    assert_eq!(s.call(5, &map(&[], Some(b"a"))), Some(0), "queued, and the board comes up");
    assert_eq!(s.call(5, &map(&[], Some(b"b"))), Some(-5), "the queue holds one");
    s.settle_init();
    s.irq();
    assert_eq!(s.events(3), 1);
    assert_eq!(s.fake.events.iter().find(|e| e.0 == 3).unwrap().1, b"a".to_vec());
}

#[test]
fn stop_powers_down_and_a_later_request_brings_the_board_up_again() {
    let mut s = Sim::new();
    s.start("rx pwr=21 pwrlvl=1");
    s.settle();
    s.call(6, &[]);
    assert_eq!(s.events(2), 1);
    assert_eq!(s.fake.spi_closed, 1);
    assert_eq!(s.fake.gpio.last(), Some(&(21, 0)));
    s.call(1, &[]);
    s.settle();
    assert_eq!(s.fake.spi_opened, 2);
    assert!(s.logged("probe ok"));
}

#[test]
fn the_host_stop_releases_the_radio() {
    let mut s = receiving();
    let (ret, _) = s.radio.handle(&mut s.fake, 0, 2, &[]);
    assert_eq!(ret, 0);
    assert!(s.fake.alarms.is_empty());
}

// ---- CAD ----

#[test]
fn cad_windows_follow_each_other_and_a_detection_opens_a_receive_window() {
    let mut s = Sim::new();
    s.start("rx cad=1 cad_interval=100 cad_rx_ms=300");
    s.settle();
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_CAD);
    // quiet window: a pause, then the next window
    s.fake.irq127 = SX127X_IRQ_CAD_DONE;
    s.irq();
    assert_eq!(s.fake.alarms.back().map(|a| a.1), Some(25), "the pause between windows");
    s.tick();
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_CAD, "the next window");
    // a detection
    s.fake.irq127 = SX127X_IRQ_CAD_DONE | SX127X_IRQ_CAD_DETECTED;
    s.irq();
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_RX_SINGLE);
    assert_eq!(s.fake.alarms.back().map(|a| a.1), Some(300));
    s.fake.irq127 = SX127X_IRQ_RX_DONE;
    s.fake.rx_packet = Some(b"cad".to_vec());
    s.irq();
    assert_eq!(s.fake.packets.len(), 1);
    s.tick();
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_CAD, "back to listening after the pause");
}

#[test]
fn a_cad_window_that_times_out_starts_the_next() {
    let mut s = Sim::new();
    s.start("rx cad=1");
    s.settle();
    s.tick();                       // the window did not finish: pause
    let before = s.fake.ops.len();
    s.tick();                       // then the next window
    assert!(s.fake.ops.len() > before);
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_CAD);
}

// ---- SX126x ----

#[test]
fn sx126x_receive_configures_and_starts_rx() {
    let mut s = Sim::sx126x();
    s.start("rx chip=2 busy=13 rst=12 cs=8 irq=14 spi=2");
    s.settle();
    assert!(s.fake.ops.contains(&SX126X_CMD_SET_RX), "SetRx");
    assert!(!s.fake.ops.contains(&SX126X_CMD_SET_TX));
    assert_eq!(s.events(1), 1);
}

#[test]
fn a_tcxo_gets_five_milliseconds_before_the_calibration() {
    let mut s = Sim::sx126x();
    s.start("rx chip=2 tcxo=1800 rst=12");
    s.settle();
    assert_eq!(s.fake.delays, std::vec![10, 20, 5]);
}

#[test]
fn sx126x_tx_resets_the_chip_and_returns_to_rx() {
    let mut s = Sim::sx126x();
    s.start("rx chip=2 rst=12");
    s.settle();
    s.fake.ops.clear();
    s.fake.gpio.clear();
    s.fake.delays.clear();
    s.call(5, &map(&[], Some(b"hello")));
    s.settle_init();                  // 30 ms low, 20 ms high, then the setup and the transmission
    assert_eq!(s.fake.delays[..2], [30, 20]);
    assert!(s.fake.ops.contains(&SX126X_CMD_SET_TX));
    // TxDone was already latched by the fake's SetTx; the interrupt reports it
    s.irq();
    assert_eq!(s.events(3), 1);
    assert_eq!(*s.fake.ops.last().unwrap(), SX126X_CMD_SET_RX, "receiving again");
}

#[test]
fn an_sx126x_transmission_that_times_out_is_aborted_with_the_irq_bits() {
    let mut s = Sim::sx126x();
    s.start("rx chip=2");
    s.settle();
    s.call(5, &map(&[], Some(b"x")));
    s.settle_init();
    s.fake.irq126 = 0;               // the fake latched TxDone at SetTx: clear it, the chip never finished
    assert!(s.tick(), "the safety timeout");
    assert_eq!(s.events(6), 1);
    assert_eq!(s.events(3), 0);
    assert_eq!(*s.fake.ops.last().unwrap(), SX126X_CMD_SET_RX, "receiving again");
}

#[test]
fn an_sx126x_packet_is_delivered() {
    let mut s = Sim::sx126x();
    s.start("rx chip=2");
    s.settle();
    s.fake.irq126 = SX126X_IRQ_RX_DONE;
    s.fake.rx_packet = Some(b"sx126".to_vec());
    s.irq();
    assert_eq!(s.fake.packets.len(), 1);
    assert_eq!(s.fake.packets[0].0, b"sx126".to_vec());
}

// ---- FSK ----

#[test]
fn fsk_receive_resets_the_chip_in_steps_and_delivers_a_packet() {
    let mut s = Sim::new();
    s.start("fsk");
    s.settle();
    assert_eq!(s.fake.delays[2..], [10, 20, 1, 1]);
    assert_eq!(s.fake.op_mode() & 7, SX127X_MODE_RX_CONTINUOUS);
    assert_eq!(s.fake.op_mode() & SX127X_LONG_RANGE, 0, "FSK bank");
    s.fake.irq127_fsk2 = SX127X_FSK_IRQ2_PAYLOAD_READY;
    s.fake.rx_packet = Some(b"12345678".to_vec());
    s.irq();
    assert_eq!(s.events(22), 1);
    assert_eq!(s.fake.packets.len(), 1);
}

#[test]
fn an_fsk_transmission_waits_in_fstx_then_sends() {
    let mut s = Sim::new();
    s.start("fsk text=hello");
    s.settle();
    s.irq();
    assert_eq!(s.events(3), 1);
    assert_eq!(s.events(20), 1);
}

/// The state block the image header asks for (Cargo.toml, `mkmod = "... --state N"`) is the one the code was built for.
#[test]
fn the_cargo_toml_asks_for_the_state_block_the_code_needs() {
    let toml = include_str!("../Cargo.toml");
    let want = std::format!("--state {}", crate::xip::STATE_BYTES);
    assert!(toml.contains(&want), "Cargo.toml must say `{want}`");
}
