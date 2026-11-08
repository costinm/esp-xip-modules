//! Direct packet filesystem service. The runtime routes component 15 to its
//! registered owner (normally mod_flash); it does not own storage state.
pub const FILES_COMPONENT: u32 = 15;
pub const FILES_LOCAL_MAX: u32 = 1024;
pub const FILES_MAX_FILE: u32 = 65504;
pub const FILES_GET: u32 = 17;
pub const FILES_PUT: u32 = 18;
pub const FILES_LOG_PUT: u32 = 19;
pub const FILES_LOG_GET: u32 = 20;
