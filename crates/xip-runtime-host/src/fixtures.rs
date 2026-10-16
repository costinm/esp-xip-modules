//! Fixtures: what the outside world looks like to a module. A chip select can be a loopback (MOSI wired to MISO),
//! a scripted device, or a recording replayed like a `tcpdump` of the bus.
use std::cell::RefCell;
use std::rc::Rc;

/// One SPI device, selected by its chip-select pin. `tx` is what the module clocks out, `rx` receives what the device
/// answers in the same transfer (same length).
pub trait SpiDevice {
    /// `Err` is the (negative) ABI error `spi_transfer` returns.
    fn transfer(&mut self, now_ms: u64, tx: &[u8], rx: &mut [u8]) -> Result<(), i32>;
}

/// MOSI wired to MISO: what the C simulator did.
pub struct Loopback;
impl SpiDevice for Loopback {
    fn transfer(&mut self, _now: u64, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
        rx.copy_from_slice(tx);
        Ok(())
    }
}

/// A device made of a closure.
pub struct Scripted<F: FnMut(u64, &[u8], &mut [u8]) -> Result<(), i32>>(pub F);
impl<F: FnMut(u64, &[u8], &mut [u8]) -> Result<(), i32>> SpiDevice for Scripted<F> {
    fn transfer(&mut self, now: u64, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
        (self.0)(now, tx, rx)
    }
}

/// One transfer of a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiRecord {
    pub t_ms: u64,
    pub tx: Vec<u8>,
    pub rx: Vec<u8>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// A recording as text: one `t=<ms> tx=<hex> rx=<hex>` line per transfer, easy to diff and to edit by hand.
pub fn save(records: &[SpiRecord]) -> String {
    records.iter().map(|r| format!("t={} tx={} rx={}\n", r.t_ms, hex(&r.tx), hex(&r.rx))).collect()
}

/// The inverse of [`save`]. Blank lines and `#` comments are ignored.
pub fn load(text: &str) -> Result<Vec<SpiRecord>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (mut t, mut tx, mut rx) = (None, None, None);
        for part in line.split_whitespace() {
            match part.split_once('=') {
                Some(("t", v)) => t = v.parse().ok(),
                Some(("tx", v)) => tx = unhex(v),
                Some(("rx", v)) => rx = unhex(v),
                _ => {}
            }
        }
        match (t, tx, rx) {
            (Some(t_ms), Some(tx), Some(rx)) => out.push(SpiRecord { t_ms, tx, rx }),
            _ => return Err(format!("line {}: expected t=<ms> tx=<hex> rx=<hex>", n + 1)),
        }
    }
    Ok(out)
}

/// Wraps a device and records every transfer it sees (capture from a simulated device, or from a real one through a
/// small module that forwards to hardware).
pub struct Recorder<D: SpiDevice> {
    pub inner: D,
    pub log: Rc<RefCell<Vec<SpiRecord>>>,
}
impl<D: SpiDevice> SpiDevice for Recorder<D> {
    fn transfer(&mut self, now: u64, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
        let r = self.inner.transfer(now, tx, rx);
        self.log.borrow_mut().push(SpiRecord { t_ms: now, tx: tx.to_vec(), rx: rx.to_vec() });
        r
    }
}

/// Plays a recording back. Each transfer must match the next recorded `tx` (a module that behaves differently than when
/// it was recorded is a failure, `ERR_IO`, reported in [`Replay::mismatch`]); the recorded `rx` is returned. With
/// `lenient`, a different `tx` is only noted and the recorded answer is still given.
pub struct Replay {
    records: Vec<SpiRecord>,
    next: usize,
    lenient: bool,
    pub mismatch: Rc<RefCell<Vec<String>>>,
}
impl Replay {
    pub fn new(records: Vec<SpiRecord>, lenient: bool) -> Replay {
        Replay { records, next: 0, lenient, mismatch: Rc::default() }
    }
    /// Transfers not consumed yet.
    pub fn remaining(&self) -> usize {
        self.records.len() - self.next
    }
}
impl SpiDevice for Replay {
    fn transfer(&mut self, _now: u64, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
        let Some(r) = self.records.get(self.next) else {
            self.mismatch.borrow_mut().push(format!("transfer {} past the end of the recording", self.next));
            return Err(xip_abi::ERR_IO);
        };
        self.next += 1;
        if r.tx != tx || r.rx.len() != rx.len() {
            self.mismatch.borrow_mut().push(format!("transfer {}: sent {} expected {}", self.next - 1, hex(tx), hex(&r.tx)));
            if !self.lenient {
                return Err(xip_abi::ERR_IO);
            }
        }
        let n = rx.len().min(r.rx.len());
        rx[..n].copy_from_slice(&r.rx[..n]);
        Ok(())
    }
}
