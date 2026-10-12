//! The slot table, the loader and the events (C: `components/espmod_host`, `espmod_loader`).

/// One module slot: `espmod_slot_t`. Build the table with [`slot!`](crate::slot) so `tools/mkmod.py --slots` can read it.
#[repr(C)]
pub struct Slot {
    name: *const u8,
    tag: u16,
    offset: u32,
    size: u32,
}

// The name is a string literal.
unsafe impl Sync for Slot {}

impl Slot {
    /// `name` must end with NUL; use the macro.
    pub const fn new(name: *const u8, tag: u16, offset: u32, size: u32) -> Self {
        Slot { name, tag, offset, size }
    }

    pub fn name_ptr(&self) -> *const u8 {
        self.name
    }
}

/// `slot!("lora", 43, 0x00000, 0x10000)`: name, tag (the component number of the module), offset in the module
/// partition, size (multiples of 64 KiB).
#[macro_export]
macro_rules! slot {
    ($name:literal, $tag:expr, $offset:expr, $size:expr) => {
        $crate::Slot::new(concat!($name, "\0").as_ptr(), $tag, $offset, $size)
    };
}

/// `espmod_slot_status_t`.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct SlotStatus {
    pub installed: bool,
    pub running: bool,
    pub version: u32,
    pub last_result: i32,
    pub runs: u32,
    pub last_runtime_ms: u32,
    pub stack_high_water_bytes: u32,
    pub last_check: i32,
}

/// An event a module emitted: `(module, event id, value type, value)`; called on the host worker, return 0.
pub type EventHandler = fn(&str, u16, u8, &[u8]) -> i32;

/// What the application offers its modules through the ABI beyond the hardware.
#[derive(Clone, Copy, Default)]
pub struct Features {
    /// `status`, `start`, `stop` of module slots for the console module (links `espmod_loader`'s by-name calls).
    pub modules: bool,
    /// `wifi_*`, `loader_*` and installing uploads, on esp-wifi-loader's C component (needs the `network` feature).
    pub network: bool,
}

#[cfg(target_os = "espidf")]
mod imp {
    use super::*;
    use core::ffi::{c_char, CStr};

    static mut HANDLER: Option<EventHandler> = None;

    /// `espmod_host_ext_t`.
    #[repr(C)]
    struct HostExt {
        wifi_start: Option<unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> i32>,
        wifi_stop: Option<unsafe extern "C" fn()>,
        wifi_status: Option<unsafe extern "C" fn(*mut i32, *mut c_char, *mut c_char, *mut i32) -> i32>,
        loader_start: Option<unsafe extern "C" fn(*const u8, usize) -> i32>,
        loader_stop: Option<unsafe extern "C" fn()>,
        modules_info: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
        module_start: Option<unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> i32>,
        module_stop: Option<unsafe extern "C" fn(*const u8, usize) -> i32>,
    }

    /// `espmod_loader_config_t`.
    #[repr(C)]
    struct LoaderConfig {
        slots: *const Slot,
        slot_count: usize,
        partition: *const u8,
        default_stack_bytes: u32,
    }

    extern "C" {
        fn espmod_host_init_simple(emit: extern "C" fn(*const c_char, u16, u8, *const u8, usize) -> i32, ext: *const HostExt) -> i32;
        fn espmod_loader_init(config: *const LoaderConfig) -> i32;
        fn espmod_loader_find(name: *const u8) -> i32;
        fn espmod_loader_slot_count() -> usize;
        fn espmod_loader_start(slot: i32, request: *const u8, len: usize) -> i32;
        fn espmod_loader_stop(slot: i32, timeout_ms: u32) -> i32;
        fn espmod_loader_request(slot: i32, component: i32, method: i32, data: *const u8, len: usize, response: *mut u8,
                                 capacity: usize, response_len: *mut usize, timeout_ms: u32) -> i32;
        fn espmod_loader_status(slot: i32, out: *mut SlotStatus);
        fn espmod_loader_modules_info(out: *mut u8, capacity: usize) -> i32;
        fn espmod_loader_start_named(name: *const u8, name_len: usize, request: *const u8, request_len: usize) -> i32;
        fn espmod_loader_stop_named(name: *const u8, name_len: usize) -> i32;
    }

    #[cfg(feature = "network")]
    extern "C" {
        fn esp_wifi_loader_wifi_start(ssid: *const u8, ssid_len: usize, psk: *const u8, psk_len: usize) -> i32;
        fn esp_wifi_loader_wifi_stop();
        fn esp_wifi_loader_wifi_status(state: *mut i32, ssid: *mut c_char, ip: *mut c_char, rssi: *mut i32) -> i32;
        fn esp_wifi_loader_server_start(name: *const u8, name_len: usize) -> i32;
        fn esp_wifi_loader_server_stop();
        fn espmod_install_init() -> i32;
    }

    extern "C" fn on_emit(module: *const c_char, id: u16, vtype: u8, value: *const u8, len: usize) -> i32 {
        let Some(handler) = (unsafe { HANDLER }) else { return 0 };
        if module.is_null() || (value.is_null() && len != 0) {
            return -1;
        }
        let module = unsafe { CStr::from_ptr(module) }.to_str().unwrap_or("");
        let value = if len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(value, len) } };
        handler(module, id, vtype, value)
    }

    pub fn init(slots: &'static [Slot], partition: &'static str, features: Features, on_event: Option<EventHandler>) -> i32 {
        debug_assert!(partition.ends_with('\0'));
        let mut ext = HostExt {
            wifi_start: None, wifi_stop: None, wifi_status: None, loader_start: None, loader_stop: None,
            modules_info: None, module_start: None, module_stop: None,
        };
        if features.modules {
            ext.modules_info = Some(espmod_loader_modules_info);
            ext.module_start = Some(espmod_loader_start_named);
            ext.module_stop = Some(espmod_loader_stop_named);
        }
        #[cfg(feature = "network")]
        if features.network {
            ext.wifi_start = Some(esp_wifi_loader_wifi_start);
            ext.wifi_stop = Some(esp_wifi_loader_wifi_stop);
            ext.wifi_status = Some(esp_wifi_loader_wifi_status);
            ext.loader_start = Some(esp_wifi_loader_server_start);
            ext.loader_stop = Some(esp_wifi_loader_server_stop);
        }
        unsafe {
            HANDLER = on_event;
            let rc = espmod_host_init_simple(on_emit, &ext);
            if rc != 0 {
                return rc;
            }
            let rc = espmod_loader_init(&LoaderConfig { slots: slots.as_ptr(), slot_count: slots.len(), partition: partition.as_ptr(),
                                                        default_stack_bytes: 8192 });
            #[cfg(feature = "network")]
            if rc == 0 && features.network {
                return espmod_install_init();
            }
            rc
        }
    }
    pub fn find(name: &str) -> Option<usize> {
        let mut c = [0u8; 24];
        if name.len() >= c.len() {
            return None;
        }
        c[..name.len()].copy_from_slice(name.as_bytes());
        let slot = unsafe { espmod_loader_find(c.as_ptr()) };
        (slot >= 0).then_some(slot as usize)
    }
    pub fn slot_count() -> usize { unsafe { espmod_loader_slot_count() } }
    pub fn status(slot: usize) -> SlotStatus {
        let mut out = SlotStatus::default();
        unsafe { espmod_loader_status(slot as i32, &mut out) };
        out
    }
    pub fn start(slot: usize, request: &[u8]) -> i32 {
        unsafe { espmod_loader_start(slot as i32, request.as_ptr(), request.len()) }
    }
    pub fn stop(slot: usize, timeout_ms: u32) -> i32 { unsafe { espmod_loader_stop(slot as i32, timeout_ms) } }
    pub fn request_raw(slot: usize, component: i32, method: i32, data: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
        /// How long a request to a module may take (the module answers within one handler call).
        const REQUEST_TIMEOUT_MS: u32 = 1000;
        let mut len = 0usize;
        let rc = unsafe {
            espmod_loader_request(slot as i32, component, method, data.as_ptr(), data.len(), answer.as_mut_ptr(),
                                  answer.len(), &mut len, REQUEST_TIMEOUT_MS)
        };
        if rc == 0 { Ok(len) } else { Err(rc) }
    }
}

#[cfg(not(target_os = "espidf"))]
mod imp {
    use super::*;

    pub fn init(_: &'static [Slot], _: &'static str, _: Features, _: Option<EventHandler>) -> i32 { -1 }
    pub fn find(_: &str) -> Option<usize> { None }
    pub fn slot_count() -> usize { 0 }
    pub fn status(_: usize) -> SlotStatus { SlotStatus::default() }
    pub fn start(_: usize, _: &[u8]) -> i32 { -1 }
    pub fn stop(_: usize, _: u32) -> i32 { -1 }
    pub fn request_raw(_: usize, _: i32, _: i32, _: &[u8], _: &mut [u8]) -> Result<usize, i32> { Err(-1) }
}

/// Start the host (its worker task) and the loader over `slots` in the data partition `partition` (NUL-terminated name,
/// e.g. `"data\0"`); `on_event` receives what modules emit. NVS must be initialised. 0 or an `esp_err_t`.
pub fn init(slots: &'static [Slot], partition: &'static str, features: Features, on_event: Option<EventHandler>) -> i32 {
    imp::init(slots, partition, features, on_event)
}
/// The slot of a module name.
pub fn find(name: &str) -> Option<usize> { imp::find(name) }
pub fn slot_count() -> usize { imp::slot_count() }
pub fn status(slot: usize) -> SlotStatus { imp::status(slot) }
/// Start the module with `request` as its start request; returns at once, the handler runs on the host worker.
pub fn start(slot: usize, request: &[u8]) -> i32 { imp::start(slot, request) }
/// Ask the module to end and wait until it has (0), e.g. before its slot is erased.
pub fn stop(slot: usize, timeout_ms: u32) -> i32 { imp::stop(slot, timeout_ms) }
/// Stop every module; true when all returned.
pub fn stop_all(timeout_ms: u32) -> bool {
    (0..slot_count()).fold(true, |ok, slot| stop(slot, timeout_ms) == 0 && ok)
}
/// One request to a running module; its answer (bytes the module defines) or the loader's error.
pub fn request_raw(slot: usize, component: i32, method: i32, data: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
    imp::request_raw(slot, component, method, data, answer)
}

/// One call to the module named `module`: start it first with `start_request` (its own convention: what brings it up
/// without asking it to do anything) when it is not running, then send `request` to `(component, method)`.
pub fn call(module: &str, start_request: &[u8], component: i32, method: i32, request: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
    let slot = find(module).ok_or(espmod_abi::ERR_NOTFOUND)?;
    if !status(slot).running {
        let rc = start(slot, start_request);
        if rc != 0 {
            return Err(rc);
        }
    }
    request_raw(slot, component, method, request, answer)
}
