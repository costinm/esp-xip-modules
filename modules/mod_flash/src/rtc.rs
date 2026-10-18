//! The RTC handoff bytes shared by Stage2, the loader and the application. They sit in the bootloader's
//! retained RTC block (`rtc_retain_mem_t`): 12 fixed bytes, then 32 custom bytes. The application writes
//! the handoff byte and restarts; Stage2 reads and clears it.
//!
//! The block's address is chip specific (`SOC_RTC_DRAM_HIGH - 48` on the ESP32, ESP32-S3 and
//! ESP32-C6, with `SOC_RTC_DRAM_HIGH` 0x3ff82000, 0x60100000 and 0x50004000); the firmware owns it.

/// Size of the retained block rounded as the bootloader does: 12 + 32 + 4 CRC, 8-aligned.
pub(crate) const RETAIN_RAW_SIZE: usize = 48;
/// Where the 32 custom bytes start inside the retained block.
pub(crate) const CUSTOM_OFFSET: usize = 12;
/// The one-shot boot request (offset from the start of the retained block).
pub(crate) const HANDOFF_OFFSET: usize = CUSTOM_OFFSET + 5;

/// Next boot: run the loader (an upgrade request).
pub(crate) const HANDOFF_LOADER: u8 = 1;
