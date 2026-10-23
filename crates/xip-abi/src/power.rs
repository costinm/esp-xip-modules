//! The `power` component: sleep and what the power code is doing, from `get_component("power")`.
//!
//! The runtime owns the sequence that makes light sleep work with the radio (DMesh's tested one): the radio is stopped the way an epoch ends
//! (the ESP-NOW callback goes inert, the station leaves, the driver stops; on the classic ESP32 it is released), the timer wake is armed, the CPU
//! light-sleeps, the UART receive is re-armed, the radio is brought back with the same settings, and the modules get `HOST_WAKE`
//! (an alarm with `ALARM_WINDOW`). A module decides *when* (policy, cadence, what to do with power save); the runtime does the radio and the power calls.
//!
//! ```c
//! const xip_power *power = host->get_component(host->user, "power");
//! xip_power_sleep req = { .size = sizeof req, .duration_ms = 500, .cycles = 4, .awake_ms = 100 };
//! power->sleep(host->user, &req);   /* returns at once; the sleeps run on the host worker, each ends with HOST_WAKE */
//! ```
use core::{ffi::c_void, ffi::c_int};

/// `Power::version`.
pub const POWER_VERSION: u32 = 1;

/// `PowerInfo::state`: nothing running.
pub const POWER_IDLE: u8 = 0;
/// In light sleep (never observed from outside: the worker is the one sleeping), or about to.
pub const POWER_SLEEPING: u8 = 1;
/// Awake again, bringing the radio back.
pub const POWER_RESUMING: u8 = 2;
/// Awake between the cycles of a request (`awake_ms`).
pub const POWER_AWAKE: u8 = 3;

/// A request for timer light sleep, for [`Power::sleep`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PowerSleep {
    /// `sizeof(xip_power_sleep)`, set by the caller.
    pub size: u32,
    /// Length of each sleep, milliseconds (at least 10).
    pub duration_ms: u32,
    /// How many sleeps; 0 is one.
    pub cycles: u16,
    /// Awake time between two sleeps, milliseconds, so modules run (0: 50).
    pub awake_ms: u16,
}
impl Default for PowerSleep {
    fn default() -> Self {
        PowerSleep { size: core::mem::size_of::<PowerSleep>() as u32, duration_ms: 0, cycles: 0, awake_ms: 0 }
    }
}
const _: () = assert!(core::mem::size_of::<PowerSleep>() == 12);

/// What the power code and FreeRTOS say, for [`Power::info`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PowerInfo {
    /// `sizeof(xip_power_info)`, set by the caller.
    pub size: u32,
    /// `POWER_*`.
    pub state: u8,
    /// ESP-IDF power management is configured (frequency scaling; light sleep is explicit unless `auto_light_sleep`).
    pub pm_configured: u8,
    /// Automatic light sleep when idle (`esp_pm_config_t::light_sleep_enable`).
    pub auto_light_sleep: u8,
    /// FreeRTOS tickless idle is built in (`CONFIG_FREERTOS_USE_TICKLESS_IDLE`).
    pub tickless_idle: u8,
    pub cpu_mhz: u16,
    pub min_mhz: u16,
    pub max_mhz: u16,
    /// FreeRTOS tick rate, Hz.
    pub tick_hz: u16,
    /// Sleeps still to run for the request.
    pub cycles_left: u16,
    /// Why the last sleep ended (`esp_sleep_wakeup_cause_t`: 0 none, 4 timer, 7 GPIO, 8 UART).
    pub wake_cause: u8,
    /// Whether promiscuous receive is on.
    pub promiscuous: u8,
    /// Sleeps attempted, entered (returned from `esp_light_sleep_start` without error) and rejected (returned an error at once: a power-management lock).
    pub sleep_attempts: u32,
    pub sleep_entries: u32,
    pub sleep_skipped: u32,
    /// The last sleep: microseconds asked, microseconds it lasted, and the radio's way back (microseconds from wake to the radio started).
    pub last_requested_us: u32,
    pub last_duration_us: u32,
    pub last_resume_us: u32,
    /// FreeRTOS: the number of tasks, and the tick count since boot.
    pub tasks: u32,
    pub ticks: u32,
    /// Internal heap: free, lowest ever, largest block.
    pub heap_free: u32,
    pub heap_min: u32,
    pub heap_largest: u32,
    /// Frames the early receive filter dropped, and promiscuous frames delivered.
    pub filter_dropped: u32,
    pub promisc_frames: u32,
    /// How the last sleep was done: 1 *soft*, 0 physical. A sleep is soft while the console is on USB-Serial/JTAG with a host attached: light sleep powers that
    /// port down and, on the bench, the host did not recover the device (it stayed half-enumerated or vanished until a replug). The radio is stopped and
    /// restarted around a plain delay and the CPU does not sleep, as DMesh's `sleepy-soft` (`usb_jtag_debug_hold`). A device on USB is charged by the host, so
    /// the saving does not matter there; unplugged, or with a UART console, the sleep is physical. Details: `components/xip_runtime/src/xip_power.c`.
    pub soft_sleep: u8,
    /// A UART/wake activity interval is still open. Informational: the module
    /// owns sleep policy. Uses one formerly reserved byte, keeping ABI size.
    pub active_window: u8,
    pub reserved: [u8; 2],
}
impl Default for PowerInfo {
    fn default() -> Self {
        let mut i: PowerInfo = unsafe { core::mem::zeroed() };
        i.size = core::mem::size_of::<PowerInfo>() as u32;
        i
    }
}
const _: () = assert!(core::mem::size_of::<PowerInfo>() == 76);

/// The `power` component table, from `get_component("power")`. The functions take no context argument other than `ctx` and never block.
#[repr(C)]
pub struct Power {
    /// `POWER_VERSION`.
    pub version: u32,
    /// `sizeof(xip_power)`: members past it do not exist.
    pub size: u32,
    /// Sleep for `duration_ms`, `cycles` times with `awake_ms` awake between, on the host worker; returns at once. Each wake delivers `HOST_WAKE` to the modules with
    /// an `ALARM_WINDOW` alarm. `ERR_BUSY` while a request runs, `ERR_ARG` for a duration under 10 ms or a size that is too small. With the radio on it is stopped
    /// for each sleep and brought back after it with the same settings (the `espnow` link goes `DOWN` and `UP`); nothing else is touched. On USB-Serial/JTAG
    /// the port drops across a sleep.
    pub sleep: Option<unsafe extern "C" fn(ctx: *mut c_void, request: *const PowerSleep) -> c_int>,
    /// Read the power state and the FreeRTOS numbers (set `info.size` first). `ERR_SIZE` if `size` is too small.
    pub info: Option<unsafe extern "C" fn(ctx: *mut c_void, info: *mut PowerInfo) -> c_int>,
    /// Print the power-management locks to the device log (`esp_pm_dump_locks`): who keeps the CPU frequency up or sleep out.
    pub dump_locks: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// Print the FreeRTOS tasks to the device log: name, priority, stack left.
    pub dump_tasks: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
}
