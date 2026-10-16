//! What the radio machine needs from the board, in the shapes of embedded-hal 1.0 (`SpiDevice::transfer`,
//! `OutputPin::set_high/low`, `InputPin::is_high`) plus the pieces of the event-driven host: a one-shot alarm that
//! calls the module back, and the interrupt line of the radio. There is no delay and no wait: the machine returns
//! and the host calls it again.
//!
//! No crate dependency: [`crate::xip::XipHal`] implements it over the esp-xip-modules host table, the unit
//! tests over a scripted fake. Results keep the integer codes the module has always reported (0 = ok).

/// SPI device wiring handed to [`Hal::spi_open`].
#[derive(Clone, Copy)]
pub struct SpiSetup {
    pub bus: i32,
    pub sck: i32,
    pub miso: i32,
    pub mosi: i32,
    pub cs: i32,
    pub hz: u32,
    pub mode: i32,
    /// SX126x BUSY (-1 = none): the host waits for it to go low before and after every transfer.
    pub ready_pin: i32,
}

pub trait Hal {
    /// Open the SPI device; the module has exactly one. 0 or a negative error.
    fn spi_open(&mut self, setup: &SpiSetup) -> i32;
    /// One chip-select cycle: `rx` receives as many bytes as `tx` sends (`rx.len() >= tx.len()`).
    fn spi(&mut self, tx: &[u8], rx: &mut [u8]) -> i32;
    /// Release the SPI device (`stop`).
    fn spi_close(&mut self);

    /// Configure `pin` as an output and drive `level`.
    fn gpio_output(&mut self, pin: i32, level: i32) -> i32;
    fn gpio_write(&mut self, pin: i32, level: i32) -> i32;
    /// The level (0/1), or a negative error.
    fn gpio_read(&mut self, pin: i32) -> i32;

    /// Report rising edges of `pin` as calls of the module (`M_IRQ`); registered once, masked with `irq_enable`.
    fn irq_attach(&mut self, pin: i32) -> i32;
    fn irq_enable(&mut self, pin: i32, on: bool) -> i32;

    /// Call the module back (`M_TIMER`) after `ms`. The module keeps at most one such alarm; returns its id (>= 0).
    fn alarm(&mut self, ms: u32) -> i32;
    fn cancel(&mut self, id: i32);

    /// A received radio packet, delivered to the application.
    fn packet(&mut self, data: &[u8], rssi: i16, snr: i8) -> i32;
    fn log(&mut self, text: &[u8]);
    /// An event for the application: `value` is raw bytes (empty = no value).
    fn event(&mut self, id: u16, value: &[u8]);
}

pub(crate) fn event_i32<H: Hal>(host: &mut H, id: u16, value: i32) {
    host.event(id, &value.to_le_bytes());
}

/// Largest radio packet (both chips).
pub const MAX_PACKET: usize = 255;

/// The next step of a setup sequence that has to let the chip settle: wait `ms` and call again with `step`, or finish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Next {
    Wait(u32, u8),
    Done(i32),
}
