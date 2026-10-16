//! [`Hal`] over the esp-xip-modules host table, and the exported handler. This is the only file that sees the C ABI;
//! the machine above it does not change when the table does.

use crate::hal::{Hal, SpiSetup, MAX_PACKET};
use crate::machine::{Radio, COMPONENT, M_IRQ, M_TIMER};
use xip_abi::{cbor::Writer, CallView, Hw, Module, SpiConfig};

/// Event id of a received packet: a CBOR array `[rssi, snr, bytes]`.
pub const EV_PACKET: u16 = 10;

/// The inverse of what `packet` emits: `(rssi, snr, bytes)` of an `EV_PACKET` value, for an application that
/// receives the event (an empty packet or trailing bytes are not a packet).
pub fn decode_packet(value: &[u8]) -> Option<(i16, i8, &[u8])> {
    use xip_abi::cbor::{Item, Reader};
    let mut r = Reader::new(value);
    if r.next()? != Item::Array(3) {
        return None;
    }
    let rssi = i16::try_from(r.next()?.as_int()?).ok()?;
    let snr = i8::try_from(r.next()?.as_int()?).ok()?;
    let Item::Bytes(packet) = r.next()? else { return None };
    (r.rest().is_empty() && !packet.is_empty()).then_some((rssi, snr, packet))
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    #[test]
    fn a_packet_event_decodes() {
        // [-60, 5, h'010203']
        assert_eq!(decode_packet(&[0x83, 0x38, 0x3b, 0x05, 0x43, 1, 2, 3]), Some((-60, 5, &[1u8, 2, 3][..])));
        assert!(decode_packet(&[0x83, 0x38, 0x3b, 0x05, 0x40]).is_none(), "empty packet");
        assert!(decode_packet(&[0x82, 0x01, 0x02]).is_none());
        assert!(decode_packet(&[0x83, 0x38, 0x3b, 0x05, 0x41, 1, 0]).is_none(), "trailing bytes");
    }
}

pub struct EspmodHal<'a> {
    hw: Hw,
    /// SPI handle + 1 (the state block starts zeroed, and 0 is a valid handle).
    spi: &'a mut i32,
}

impl<'a> EspmodHal<'a> {
    pub fn new(hw: Hw, spi: &'a mut i32) -> Self {
        EspmodHal { hw, spi }
    }
}

impl Hal for EspmodHal<'_> {
    fn spi_open(&mut self, setup: &SpiSetup) -> i32 {
        let mut c = SpiConfig::new(setup.bus);
        c.sck = setup.sck;
        c.miso = setup.miso;
        c.mosi = setup.mosi;
        c.cs = setup.cs;
        c.hz = setup.hz;
        c.mode = setup.mode;
        c.ready_pin = setup.ready_pin;
        c.ready_level = 0;
        let handle = self.hw.spi_open(&c);
        if handle < 0 { return handle; }
        *self.spi = handle + 1;
        0
    }

    fn spi(&mut self, tx: &[u8], rx: &mut [u8]) -> i32 {
        if *self.spi == 0 { return -3; }
        self.hw.spi_transfer(*self.spi - 1, tx, rx)
    }

    fn spi_close(&mut self) {
        if *self.spi > 0 {
            let _ = self.hw.spi_close(*self.spi - 1);
            *self.spi = 0;
        }
    }

    fn gpio_output(&mut self, pin: i32, level: i32) -> i32 {
        self.hw.gpio_config(pin, xip_abi::GPIO_OUTPUT, xip_abi::GPIO_PULL_NONE, level)
    }

    fn gpio_write(&mut self, pin: i32, level: i32) -> i32 {
        self.hw.gpio_write(pin, level)
    }

    fn gpio_read(&mut self, pin: i32) -> i32 {
        self.hw.gpio_read(pin)
    }

    fn irq_attach(&mut self, pin: i32) -> i32 {
        self.hw.irq_register(pin, xip_abi::IRQ_RISING, COMPONENT, M_IRQ)
    }

    fn irq_enable(&mut self, pin: i32, on: bool) -> i32 {
        self.hw.irq_enable(pin, on)
    }

    fn alarm(&mut self, ms: u32) -> i32 {
        self.hw.alarm_set(ms, 0, 0, xip_abi::ALARM_EXACT, COMPONENT, M_TIMER)
    }

    fn cancel(&mut self, id: i32) {
        let _ = self.hw.alarm_cancel(id);
    }

    fn packet(&mut self, data: &[u8], rssi: i16, snr: i8) -> i32 {
        let mut out = [0u8; MAX_PACKET + 16];
        let mut w = Writer::new(&mut out);
        w.array(3).int(rssi as i64).int(snr as i64).bytes(data);
        match w.finish() {
            Some(encoded) => self.hw.emit(EV_PACKET, xip_abi::VALUE_CBOR, encoded),
            None => -3,
        }
    }

    fn log(&mut self, text: &[u8]) {
        let _ = self.hw.log(text);
    }

    fn event(&mut self, id: u16, value: &[u8]) {
        let kind = if value.is_empty() { xip_abi::VALUE_EMPTY } else { xip_abi::VALUE_BYTES };
        let _ = self.hw.emit(id, kind, value);
    }
}

/// The module: the radio machine and the SPI handle. All zero is a valid start.
pub struct Lora {
    radio: Radio,
    spi: i32,
}

/// Bytes of state the image asks for (`mkmod: --state` in Cargo.toml).
pub const STATE_BYTES: usize = 768;

impl Module for Lora {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        let mut hal = EspmodHal::new(*hw, &mut self.spi);
        let (ret, answer) = self.radio.handle(&mut hal, call.component, call.method, call.data);
        if call.wants_response() {
            if let Some(rc) = answer {
                let mut buf = [0u8; 16];
                let mut w = Writer::new(&mut buf);
                w.map(1).uint(1).int(rc as i64);
                if let Some(encoded) = w.finish() { let _ = hw.respond(encoded); }
            }
        }
        ret
    }
}

/// Export `xip_handle` for this module. The flash image's `main` calls this macro; an application that links the
/// module can call `Module::handle` itself.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::xip::Lora, state = $crate::xip::STATE_BYTES);
    };
}
