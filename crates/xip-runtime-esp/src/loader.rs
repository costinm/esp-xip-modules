//! The loader (C: `components/xip_runtime`, `xip_loader`): it scans the module partition for images; there is no slot table.

/// `xip_slot_status_t`.
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

/// Kept so existing callers compile: modules no longer `emit` (they send messages, which a routing component will carry), so this is never called.
pub type EventHandler = fn(&str, u16, u8, &[u8]) -> i32;

/// The application's packet pool as the host uses it (`xip_pool_ops`): a slot is an index; `acquire` returns one with a single
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
    /// `status`, `start`, `stop` of module slots for the console module (links `xip_loader`'s by-name calls).
    pub modules: bool,
    /// The application's own device settings (`nv`, include/xip.h): called with a name, a value or NULL to read, and a buffer; returns
    /// the length read, 0 written, or -1 bad argument, -3 storage, -4 unknown, -5 buffer too small.
    pub nv: Option<unsafe extern "C" fn(*const core::ffi::c_char, usize, *const u8, usize, *mut u8, usize) -> i32>,
    /// The application's own packet pool for module packets (see [`PoolOps`]); `None`: the host's own dynamic pool.
    pub pool: Option<&'static PoolOps>,
    /// The application's own Wi-Fi owner (DMesh drives the radio itself): its functions answer the `wifi` and `wifi-tuning`
    /// components. `None`: the host's simple station owner.
    pub wifi: Option<&'static crate::WifiOwner>,
}

#[cfg(target_os = "espidf")]
mod imp {
    use super::*;
    use core::ffi::c_char;


    /// `xip_host_ext_t`.
    #[repr(C)]
    struct HostExt {
        wifi_set: Option<unsafe extern "C" fn(*const xip_abi::WifiIntent, *mut u32) -> i32>,
        wifi_get: Option<unsafe extern "C" fn(*mut xip_abi::WifiIntent, *mut u32) -> i32>,
        wifi_state: Option<unsafe extern "C" fn(*mut xip_abi::WifiState) -> i32>,
        wifi_scan: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
        nv: Option<unsafe extern "C" fn(*const c_char, usize, *const u8, usize, *mut u8, usize) -> i32>,
        pool: *const PoolOps,
        modules_info: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
        module_start: Option<unsafe extern "C" fn(*const u8, usize, *const u8, usize) -> i32>,
        module_stop: Option<unsafe extern "C" fn(*const u8, usize) -> i32>,
        module_manifest: Option<unsafe extern "C" fn(*const u8, usize, *mut u8, usize) -> i32>,
        packet: Option<unsafe extern "C" fn(*mut core::ffi::c_void, *const c_char, u32, i32, i32, u64, *const u8, usize) -> i32>,
        inbox_ready: Option<unsafe extern "C" fn(*mut core::ffi::c_void, i32)>,
        wifi_reconcile: Option<unsafe extern "C" fn()>,
        wifi_tuning_set: Option<unsafe extern "C" fn(*const xip_abi::WifiTuning) -> i32>,
        wifi_tuning_get: Option<unsafe extern "C" fn(*mut xip_abi::WifiTuning) -> i32>,
        wifi_monitor: Option<unsafe extern "C" fn(*const xip_abi::WifiMonitor) -> i32>,
    }

    /// `xip_loader_config_t`.
    #[repr(C)]
    struct LoaderConfig {
        partition: *const u8,
        default_stack_bytes: u32,
    }

    extern "C" {
        fn xip_host_init_simple(ext: *const HostExt) -> i32;
        fn xip_loader_init(config: *const LoaderConfig) -> i32;
        fn xip_loader_autostart() -> i32;
        fn xip_loader_find(name: *const u8) -> i32;
        fn xip_loader_slot_count() -> usize;
        fn xip_loader_start(slot: i32, request: *const u8, len: usize) -> i32;
        fn xip_loader_stop(slot: i32, timeout_ms: u32) -> i32;
        fn xip_loader_request(slot: i32, component: i32, method: i32, data: *const u8, len: usize, response: *mut u8,
                                 capacity: usize, response_len: *mut usize, timeout_ms: u32) -> i32;
        fn xip_loader_status(slot: i32, out: *mut SlotStatus);
        fn xip_loader_modules_info(out: *mut u8, capacity: usize) -> i32;
        fn xip_loader_start_named(name: *const u8, name_len: usize, request: *const u8, request_len: usize) -> i32;
        fn xip_loader_stop_named(name: *const u8, name_len: usize) -> i32;
        fn xip_loader_manifest_named(name: *const u8, name_len: usize, out: *mut u8, capacity: usize) -> i32;
    }

    pub fn init(partition: &'static str, features: Features, on_event: Option<EventHandler>) -> i32 {
        debug_assert!(partition.ends_with('\0'));
        crate::module_poll_esp::install();
        extern "C" {
            fn xip_power_set_radio(radio: *const PowerRadio);
        }
        /// `xip_power_radio`: the radio steps around a light sleep.
        #[repr(C)]
        struct PowerRadio {
            suspend: Option<unsafe extern "C" fn() -> bool>,
            resume: Option<unsafe extern "C" fn()>,
        }
        static POWER_RADIO: PowerRadio = PowerRadio { suspend: Some(crate::wifi::power_suspend), resume: Some(crate::wifi::power_resume) };
        if features.wifi.is_none() {
            // an application that owns the radio sleeps it itself; the built-in owner gives the sleep owner its steps
            unsafe { xip_power_set_radio(&POWER_RADIO) };
            // the boot credentials (`nv` sta_ssid / sec:sta) are the wanted state of the built-in owner
            let (mut boot, _) = crate::wifi::wanted();
            if crate::settings::load_sta(&mut boot).is_ok() { crate::wifi::set_boot(boot); }
        }
        let mut ext = HostExt {
            wifi_set: Some(crate::wifi::set), wifi_get: Some(crate::wifi::get),
            wifi_state: Some(crate::wifi_esp::abi_state), wifi_scan: Some(crate::wifi_esp::abi_scan),
            wifi_reconcile: Some(crate::wifi::reconcile), wifi_monitor: Some(crate::promisc_esp::abi_monitor),
            wifi_tuning_set: Some(crate::wifi::tuning_set), wifi_tuning_get: Some(crate::wifi::tuning_get),
            nv: features.nv.or(Some(crate::settings::nv_hook)),
            pool: features.pool.map_or(core::ptr::null(), |p| p as *const PoolOps),
            modules_info: None, module_start: None, module_stop: None, module_manifest: None, packet: None, inbox_ready: None,
        };
        if let Some(owner) = features.wifi {
            ext.wifi_set = Some(owner.set); ext.wifi_get = Some(owner.get);
            ext.wifi_tuning_set = Some(owner.tuning_set); ext.wifi_tuning_get = Some(owner.tuning_get);
            ext.wifi_state = owner.state; ext.wifi_scan = owner.scan; ext.wifi_monitor = owner.monitor;
            ext.wifi_reconcile = None;
        }
        if features.modules {
            ext.modules_info = Some(xip_loader_modules_info);
            ext.module_start = Some(xip_loader_start_named);
            ext.module_stop = Some(xip_loader_stop_named);
            ext.module_manifest = Some(xip_loader_manifest_named);
        }
        unsafe {
            let _ = on_event;
            crate::uart_frames::install(); // the serial line carries ports in PPP records, text lines are the console
            let rc = xip_host_init_simple(&ext);
            if rc != 0 {
                return rc;
            }
            let rc = xip_loader_init(&LoaderConfig { partition: partition.as_ptr(),
                                                        default_stack_bytes: 8192 });
            rc
        }
    }
    pub fn autostart() -> i32 { unsafe { xip_loader_autostart() } }
    pub fn find(name: &str) -> Option<usize> {
        let mut c = [0u8; 24];
        if name.len() >= c.len() {
            return None;
        }
        c[..name.len()].copy_from_slice(name.as_bytes());
        let slot = unsafe { xip_loader_find(c.as_ptr()) };
        (slot >= 0).then_some(slot as usize)
    }
    pub fn slot_count() -> usize { unsafe { xip_loader_slot_count() } }
    pub fn status(slot: usize) -> SlotStatus {
        let mut out = SlotStatus::default();
        unsafe { xip_loader_status(slot as i32, &mut out) };
        out
    }
    pub fn start(slot: usize, request: &[u8]) -> i32 {
        unsafe { xip_loader_start(slot as i32, request.as_ptr(), request.len()) }
    }
    pub fn stop(slot: usize, timeout_ms: u32) -> i32 { unsafe { xip_loader_stop(slot as i32, timeout_ms) } }
    pub fn manifest(module: &str, out: &mut [u8]) -> Result<usize, i32> {
        let n = unsafe { xip_loader_manifest_named(module.as_ptr(), module.len(), out.as_mut_ptr(), out.len()) };
        if n >= 0 { Ok(n as usize) } else { Err(n) }
    }
    pub fn request_raw(slot: usize, component: i32, method: i32, data: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
        /// How long a request to a module may take (the module answers within one handler call).
        const REQUEST_TIMEOUT_MS: u32 = 1000;
        let mut len = 0usize;
        let rc = unsafe {
            xip_loader_request(slot as i32, component, method, data.as_ptr(), data.len(), answer.as_mut_ptr(),
                                  answer.len(), &mut len, REQUEST_TIMEOUT_MS)
        };
        if rc == 0 { Ok(len) } else { Err(rc) }
    }
}

#[cfg(not(target_os = "espidf"))]
mod imp {
    use super::*;

    pub fn init(_: &'static str, _: Features, _: Option<EventHandler>) -> i32 { -1 }
    pub fn autostart() -> i32 { -1 }
    pub fn find(_: &str) -> Option<usize> { None }
    pub fn slot_count() -> usize { 0 }
    pub fn status(_: usize) -> SlotStatus { SlotStatus::default() }
    pub fn start(_: usize, _: &[u8]) -> i32 { -1 }
    pub fn stop(_: usize, _: u32) -> i32 { -1 }
    pub fn request_raw(_: usize, _: i32, _: i32, _: &[u8], _: &mut [u8]) -> Result<usize, i32> { Err(-1) }
    pub fn manifest(_: &str, _: &mut [u8]) -> Result<usize, i32> { Err(-1) }
}

/// Start the host (its worker task) and the loader over the data partition `partition` (NUL-terminated name,
/// e.g. `"data\0"`); `on_event` is unused (modules send messages now). NVS must be initialised. 0 or an `esp_err_t`.
pub fn init(partition: &'static str, features: Features, on_event: Option<EventHandler>) -> i32 {
    imp::init(partition, features, on_event)
}
/// Start the installed modules whose manifest asks to run at boot (a console). The application calls it last, once everything a
/// module may use is up; an application that owns the serial port itself (DMesh in `uart.mode=ppp`) does not call it. Few modules should ask.
pub fn autostart() -> i32 { imp::autostart() }
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
/// The CBOR manifest of an installed module (`module_manifest`, include/xip.h): what it says about itself, read from flash. The way to learn
/// a module's method names and ids without compiling its crate in.
pub fn manifest(module: &str, out: &mut [u8]) -> Result<usize, i32> { imp::manifest(module, out) }
/// One request to a running module; its answer (bytes the module defines) or the loader's error.
pub fn request_raw(slot: usize, component: i32, method: i32, data: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
    imp::request_raw(slot, component, method, data, answer)
}

/// One call to the module named `module`: start it first with `start_request` (its own convention: what brings it up
/// without asking it to do anything) when it is not running, then send `request` to `(component, method)`.
pub fn call(module: &str, start_request: &[u8], component: i32, method: i32, request: &[u8], answer: &mut [u8]) -> Result<usize, i32> {
    let slot = find(module).ok_or(xip_abi::ERR_NOTFOUND)?;
    if !status(slot).running {
        let rc = start(slot, start_request);
        if rc != 0 {
            return Err(rc);
        }
    }
    request_raw(slot, component, method, request, answer)
}
