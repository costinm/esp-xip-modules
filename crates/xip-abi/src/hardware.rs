//! The `hw` component: pins, interrupts, SPI, I2C, ADC and the RGB LED, from `get_component("hw")`. Valid for the firmware lifetime; every function
//! takes `ctx` first (the `user` of the host table) and never blocks. A host that has none of it returns `NULL`; a member a host leaves empty is `None`.
//! A pin or a bus belongs to the module that configured it until the instance is destroyed. `Hw::gpio_*`, `irq_*`, `spi_*`, `i2c_transfer`, `adc_read`
//! and `rgbled_write` wrap these for Rust modules.
use core::ffi::c_void;

use crate::SpiConfig;

/// `Hardware::version`.
pub const HARDWARE_VERSION: u32 = 1;

/// The `hw` component table.
#[repr(C)]
pub struct Hardware {
    /// `HARDWARE_VERSION`.
    pub version: u32,
    /// `sizeof(xip_hardware)`: members past it do not exist.
    pub size: u32,
    /// Configure a pin.
    ///
    /// Also sets the initial level of an output. The pin belongs to the module until the instance is destroyed.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_BUSY` if another module owns it, `ERR_UNSUPPORTED` if main reserved it.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): GPIO number.
    /// * `mode` (`int`): `GPIO_*`.
    /// * `pull` (`int`): `GPIO_PULL_*`.
    /// * `level` (`int`): Initial output level.
    pub gpio_config: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, mode: core::ffi::c_int, pull: core::ffi::c_int, level: core::ffi::c_int) -> core::ffi::c_int>,
    /// Read a pin.
    ///
    /// # Returns
    ///
    /// `int` — level 0/1, or a negative error.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): A pin configured by this module.
    pub gpio_read: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int) -> core::ffi::c_int>,
    /// Set an output level.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): A pin configured by this module.
    /// * `level` (`int`): 0 or 1.
    pub gpio_write: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, level: core::ffi::c_int) -> core::ffi::c_int>,
    /// Report the edges of a pin.
    ///
    /// An edge becomes a call `(component, method)` with `value` = the pin level. The interrupt itself never runs
    /// module code; edges that arrive while the queue is full are counted and dropped.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): The pin.
    /// * `edge` (`int`): `IRQ_*`.
    /// * `component` (`int`): Returned in the call.
    /// * `method` (`int`): Returned in the call.
    pub irq_register: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, edge: core::ffi::c_int, component: core::ffi::c_int, method: core::ffi::c_int) -> core::ffi::c_int>,
    /// Stop reporting a pin.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_NOTFOUND`.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): The pin.
    pub irq_unregister: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int) -> core::ffi::c_int>,
    /// Mask or unmask a registered pin.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_NOTFOUND`.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): The pin.
    /// * `enabled` (`int`): 1 unmask, 0 mask.
    pub irq_enable: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, enabled: core::ffi::c_int) -> core::ffi::c_int>,
    /// Open a full-duplex SPI device.
    ///
    /// The first open of a controller initialises it with the pins of the config; later opens must use the same
    /// pins. A controller that main initialised is shared as is.
    ///
    /// # Returns
    ///
    /// `int` — a handle (>= 0) or a negative error.
    ///
    /// # Arguments
    ///
    /// * `config` (`const xip_spi_config *`): See `spi_config`.
    pub spi_open: Option<unsafe extern "C" fn(ctx: *mut c_void, config: *const SpiConfig) -> core::ffi::c_int>,
    /// One chip-select cycle.
    ///
    /// `tx` and `rx` are both `len` bytes (`rx` may be NULL). Bounded; waits for `ready_pin` when configured.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_BUSY` when `ready_pin` never became ready, `ERR_IO`.
    ///
    /// # Arguments
    ///
    /// * `handle` (`int`): From `spi_open`.
    /// * `tx` (`const uint8_t *`): Bytes to send.
    /// * `rx` (`uint8_t *`): Bytes received, or NULL.
    /// * `len` (`size_t`): Length.
    pub spi_transfer: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: core::ffi::c_int, tx: *const u8, rx: *mut u8, len: usize) -> core::ffi::c_int>,
    /// Release an SPI device.
    ///
    /// Also done when the instance is destroyed.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG`.
    ///
    /// # Arguments
    ///
    /// * `handle` (`int`): From `spi_open`.
    pub spi_close: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: core::ffi::c_int) -> core::ffi::c_int>,
    /// Write then read on an I2C bus.
    ///
    /// Either direction may be empty. The pins are held for the call only.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `port` (`int`): Controller.
    /// * `sda` (`int`): Pin.
    /// * `scl` (`int`): Pin.
    /// * `frequency` (`uint32_t`): Hz.
    /// * `address` (`uint8_t`): 7-bit address.
    /// * `tx` (`const uint8_t *`): Bytes to write.
    /// * `tx_len` (`size_t`): Length.
    /// * `rx` (`uint8_t *`): Buffer for the read.
    /// * `rx_len` (`size_t`): Length.
    /// * `timeout_ms` (`uint32_t`): Longest wait.
    pub i2c_transfer: Option<unsafe extern "C" fn(ctx: *mut c_void, port: core::ffi::c_int, sda: core::ffi::c_int, scl: core::ffi::c_int, frequency: u32, address: u8, tx: *const u8, tx_len: usize, rx: *mut u8, rx_len: usize, timeout_ms: u32) -> core::ffi::c_int>,
    /// One-shot 12-bit reading.
    ///
    /// `unit` and `channel` may be NULL.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): GPIO number.
    /// * `ref_mv` (`uint32_t`): Reference for the millivolt value.
    /// * `raw` (`int *`): Raw reading.
    /// * `mv` (`uint32_t *`): Millivolts.
    /// * `unit` (`int *`): ADC unit used.
    /// * `channel` (`int *`): ADC channel used.
    pub adc_read: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, ref_mv: u32, raw: *mut core::ffi::c_int, mv: *mut u32, unit: *mut core::ffi::c_int, channel: *mut core::ffi::c_int) -> core::ffi::c_int>,
    /// Set a WS2812-style LED.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `pin` (`int`): The data pin.
    /// * `red` (`uint8_t`): 0..255.
    /// * `green` (`uint8_t`): 0..255.
    /// * `blue` (`uint8_t`): 0..255.
    pub rgbled_write: Option<unsafe extern "C" fn(ctx: *mut c_void, pin: core::ffi::c_int, red: u8, green: u8, blue: u8) -> core::ffi::c_int>,
}
