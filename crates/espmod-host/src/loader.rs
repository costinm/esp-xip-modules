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

/// Six-page module layout preset. The runtime owns names, tags,
/// offsets and the state-log slot order; applications select this preset.
pub static MODULE_SLOTS: [Slot; 6] = [
    crate::slot!("lora", 43, 0x00000, 0x10000),
    crate::slot!("flash", 44, 0x10000, 0x10000),
    crate::slot!("hw", 45, 0x20000, 0x10000),
    crate::slot!("hello", 46, 0x30000, 0x10000),
    crate::slot!("test", 2, 0x40000, 0x10000),
    crate::slot!("console", 20, 0x50000, 0x10000),
];

impl Slot {
    /// `name` must end with NUL; use the macro.
    pub const fn new(name: *const u8, tag: u16, offset: u32, size: u32) -> Self {
        Slot { name, tag, offset, size }
    }

    pub fn name_ptr(&self) -> *const u8 { self.name }
    pub fn offset(&self) -> u32 { self.offset }
    pub fn size(&self) -> u32 { self.size }
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
    pub needs_reboot: bool,
}

/// An event a module emitted: `(module, event id, value type, value)`; called on the host worker, return 0.
pub type EventHandler = fn(&str, u16, u8, &[u8]) -> i32;

/// The application's packet pool as the host uses it (`espmod_pool_ops`): a slot is an index; `acquire` returns one with a single
/// reference (`reserve` slots stay free for the application), the last `release` frees it. DMesh implements it on the QUIC pool, so
/// the packets of modules and of the network come from one budget that grows and shrinks with the heap.
#[repr(C)]
pub struct PoolOps {
    pub ctx: *mut core::ffi::c_void,
    pub acquire: unsafe extern "C" fn(*mut core::ffi::c_void, u16) -> i32,
    pub retain: unsafe extern "C" fn(*mut core::ffi::c_void, i32),
    pub release: unsafe extern "C" fn(*mut core::ffi::c_void, i32),
    pub refs: unsafe extern "C" fn(*mut core::ffi::c_void, i32) -> u32,
    pub data: unsafe extern "C" fn(*mut core::ffi::c_void, i32) -> *mut u8,
    pub slot_size: unsafe extern "C" fn(*mut core::ffi::c_void) -> u16,
    pub slots: unsafe extern "C" fn(*mut core::ffi::c_void) -> u16,
    pub available: unsafe extern "C" fn(*mut core::ffi::c_void) -> u16,
}
unsafe impl Sync for PoolOps {}

/// What the application offers its modules through the ABI beyond the hardware.
#[derive(Clone, Copy, Default)]
pub struct Features {
    /// `status`, `start`, `stop` of module slots for the console module (links `espmod_loader`'s by-name calls).
    pub modules: bool,
    /// Do not start the installed modules whose manifest asks for it (a console) when the host starts: the application
    /// owns the serial port in this boot (DMesh in `uart.mode=ppp`) and starts them itself, or never.
    pub no_autostart: bool,
    /// The application's own device settings (`nv`, abi/API.md): called with a name, a value or NULL to read, and a buffer; returns
    /// the length read, 0 written, or -1 bad argument, -3 storage, -4 unknown, -5 buffer too small.
    pub nv: Option<unsafe extern "C" fn(*const core::ffi::c_char, usize, *const u8, usize, *mut u8, usize) -> i32>,
    /// The application's own packet pool for module packets (see [`PoolOps`]); `None`: the host's own dynamic pool.
    pub pool: Option<&'static PoolOps>,
    /// Offer the Rust host's simple STA Wi-Fi ABI to modules. The application
    /// must give this driver exclusive ownership of the radio while enabled.
    pub wifi: bool,
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
        wifi_scan: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
        wifi_config: Option<unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> i32>,
        wifi_saved: Option<unsafe extern "C" fn(*mut c_char, *mut i32) -> i32>,
        nv: Option<unsafe extern "C" fn(*const c_char, usize, *const u8, usize, *mut u8, usize) -> i32>,
        pool: *const PoolOps,
        modules_info: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
        module_start: Option<unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> i32>,
        module_stop: Option<unsafe extern "C" fn(*const u8, usize) -> i32>,
        module_manifest: Option<unsafe extern "C" fn(*const u8, usize, *mut u8, usize) -> i32>,
        packet: Option<unsafe extern "C" fn(*mut core::ffi::c_void, *const c_char, u32, i32, i32, u64, *const u8, usize) -> i32>,
        inbox_ready: Option<unsafe extern "C" fn(*mut core::ffi::c_void, i32)>,
        transport_reconcile: Option<unsafe extern "C" fn()>,
        transport_set: Option<unsafe extern "C" fn(*const u8, usize, *mut u32) -> i32>,
        transport_get: Option<unsafe extern "C" fn(*mut espmod_abi::BearerProfile, *mut u32) -> i32>,
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
        fn espmod_loader_autostart() -> i32;
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
        fn espmod_loader_manifest_named(name: *const u8, name_len: usize, out: *mut u8, capacity: usize) -> i32;
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
        crate::module_poll_esp::install();
        if features.wifi {
            let mut boot = crate::profile::snapshot();
            if crate::settings::load_sta(&mut boot).is_ok() {
                boot.requested_transport = Some(crate::profile::TransportKind::Sta);
                boot.run_requested = true; boot.ap = 0;
                let _ = crate::profile::set(boot);
            }
        }
        let mut ext = HostExt {
            wifi_start: None, wifi_stop: None, wifi_status: None, wifi_scan: None, wifi_config: None, wifi_saved: None,
            nv: features.nv.or(Some(crate::settings::nv_hook)),
            pool: features.pool.map_or(core::ptr::null(), |p| p as *const PoolOps),
            transport_reconcile: features.wifi.then_some(crate::wifi_hooks::reconcile),
            transport_set: Some(crate::profile::set_hook), transport_get: Some(crate::profile::get_hook),
            modules_info: None, module_start: None, module_stop: None, module_manifest: None, packet: None, inbox_ready: None,
        };
        if features.modules {
            ext.modules_info = Some(espmod_loader_modules_info);
            ext.module_start = Some(espmod_loader_start_named);
            ext.module_stop = Some(espmod_loader_stop_named);
            ext.module_manifest = Some(espmod_loader_manifest_named);
        }
        if features.wifi {
            ext.wifi_start = Some(crate::wifi_hooks::start);
            ext.wifi_stop = Some(crate::wifi_hooks::stop);
            ext.wifi_status = Some(crate::wifi_esp::abi_status);
            ext.wifi_config = Some(crate::wifi_hooks::config);
            ext.wifi_saved = Some(crate::wifi_hooks::saved);
            ext.wifi_scan = Some(crate::wifi_esp::abi_scan);
        }
        unsafe {
            HANDLER = on_event;
            let rc = espmod_host_init_simple(on_emit, &ext);
            if rc != 0 {
                return rc;
            }
            if features.wifi { crate::profile::notify_modules(crate::profile::generation()); }
            let rc = espmod_loader_init(&LoaderConfig { slots: slots.as_ptr(), slot_count: slots.len(), partition: partition.as_ptr(),
                                                        default_stack_bytes: 8192 });
            if rc == 0 && features.modules && !features.no_autostart {
                // the installed modules whose manifest asks to run at boot (a console module, if there is one)
                espmod_loader_autostart();
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
    pub fn manifest(module: &str, out: &mut [u8]) -> Result<usize, i32> {
        let n = unsafe { espmod_loader_manifest_named(module.as_ptr(), module.len(), out.as_mut_ptr(), out.len()) };
        if n >= 0 { Ok(n as usize) } else { Err(n) }
    }
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
    pub fn manifest(_: &str, _: &mut [u8]) -> Result<usize, i32> { Err(-1) }
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
/// The CBOR manifest of an installed module (`module_manifest`, abi/API.md): what it says about itself, read from flash. The way to learn
/// a module's method names and ids without compiling its crate in.
pub fn manifest(module: &str, out: &mut [u8]) -> Result<usize, i32> { imp::manifest(module, out) }
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
