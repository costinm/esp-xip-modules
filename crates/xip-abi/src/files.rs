//! Direct packet filesystem service. The runtime routes component 15 to its
//! registered owner (normally mod_flash); it does not own storage state.
pub const FILES_COMPONENT: u32 = 15;
pub const FILES_LOCAL_MAX: u32 = 1024;
pub const FILES_MAX_FILE: u32 = 65504;
pub const FILES_GET: u32 = 17;
pub const FILES_PUT: u32 = 18;
pub const FILES_LOG_PUT: u32 = 19;
pub const FILES_LOG_GET: u32 = 20;

/// Namespace IDs (wire tag 16). Missing means flash for backwards compatibility.
pub const FILES_NS_FLASH: u8 = 0;
pub const FILES_NS_RAM: u8 = 1;
pub const FILES_NS_PSRAM: u8 = 2;
pub const FILES_NS_RTC: u8 = 3;
pub const FILES_NS_PROC: u8 = 4;
pub const FILES_NS_SYS: u8 = 5;
