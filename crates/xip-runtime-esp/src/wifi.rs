//! The `wifi` and `wifi-tuning` components of this host (`xip_abi::{Wifi, WifiTuner}`).
//!
//! The wanted state is a [`WifiIntent`] and a [`WifiTuning`] with a generation, kept here. A simple station owner
//! (`wifi_esp`) makes it true on the host worker; every change tells the modules (`HOST_WIFI`). An application that drives the
//! radio itself (DMesh) hands the host a [`WifiOwner`] in [`Features`](crate::Features) instead: its functions answer the
//! component, and it calls [`changed`] when the wanted state changes.
#![cfg_attr(not(target_os = "espidf"), allow(dead_code))]
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};
use xip_abi::{WifiIntent, WifiState, WifiTuning, ERR_ARG, ERR_SIZE, ERR_UNSUPPORTED};

/// An application's own implementation of the Wi-Fi components, called from the host worker and from module handlers
/// (never blocking). The signatures are those of the C host's hooks: `set`/`get` carry the generation, `state` and `scan` may be
/// left out when the application reports neither.
pub struct WifiOwner {
    pub set: unsafe extern "C" fn(*const WifiIntent, *mut u32) -> i32,
    pub get: unsafe extern "C" fn(*mut WifiIntent, *mut u32) -> i32,
    pub tuning_set: unsafe extern "C" fn(*const WifiTuning) -> i32,
    pub tuning_get: unsafe extern "C" fn(*mut WifiTuning) -> i32,
    pub state: Option<unsafe extern "C" fn(*mut WifiState) -> i32>,
    pub scan: Option<unsafe extern "C" fn(*mut u8, usize) -> i32>,
    /// Promiscuous receive on/off (`Wifi::monitor`); `None`: `ERR_UNSUPPORTED`.
    pub monitor: Option<unsafe extern "C" fn(*const xip_abi::WifiMonitor) -> i32>,
}

/// Tell every module that the wanted Wi-Fi state or the tuning changed (`HOST_WIFI`, value = `generation`). The built-in owner
/// calls it itself; an application's [`WifiOwner`] calls it after a change.
pub fn changed(generation: u32) {
    #[cfg(target_os = "espidf")]
    unsafe {
        extern "C" {
            fn xip_host_wifi_changed(generation: u32);
        }
        xip_host_wifi_changed(generation);
    }
    #[cfg(not(target_os = "espidf"))]
    let _ = generation;
}

struct Wanted {
    intent: WifiIntent,
    tuning: WifiTuning,
    generation: u32,
}

/// The wanted state, serialized by `locked`.
struct Store {
    locked: AtomicBool,
    wanted: UnsafeCell<Option<Wanted>>,
}
unsafe impl Sync for Store {}

static STORE: Store = Store { locked: AtomicBool::new(false), wanted: UnsafeCell::new(None) };

impl Store {
    fn with<R>(&self, f: impl FnOnce(&mut Wanted) -> R) -> R {
        while self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            #[cfg(target_os = "espidf")]
            unsafe {
                esp_idf_sys::vTaskDelay(1);
            }
            #[cfg(not(target_os = "espidf"))]
            core::hint::spin_loop();
        }
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.locked);
        let slot = unsafe { &mut *self.wanted.get() };
        f(slot.get_or_insert_with(|| Wanted { intent: WifiIntent::default(), tuning: WifiTuning::default(), generation: 0 }))
    }
}

/// Every combination of the component runs here: off, ESP-NOW only, station, soft AP, soft AP + station (ESP-NOW with the radio on).
fn supported(_intent: &WifiIntent) -> Result<(), i32> {
    Ok(())
}

pub(crate) unsafe extern "C" fn set(intent: *const WifiIntent, generation: *mut u32) -> i32 {
    if intent.is_null() || generation.is_null() {
        return ERR_ARG;
    }
    let next = unsafe { *intent }.normalized();
    if let Err(e) = next.validate().and_then(|()| supported(&next)) {
        return e;
    }
    let (number, changed_now) = STORE.with(|w| {
        let changed = w.intent != next;
        if changed {
            w.intent = next;
            w.generation = w.generation.wrapping_add(1).max(1);
        }
        (w.generation, changed)
    });
    unsafe { *generation = number };
    if changed_now {
        changed(number);
    }
    0
}

pub(crate) unsafe extern "C" fn get(intent: *mut WifiIntent, generation: *mut u32) -> i32 {
    if intent.is_null() || generation.is_null() {
        return ERR_ARG;
    }
    if unsafe { (*intent).size } < core::mem::size_of::<WifiIntent>() as u32 {
        return ERR_SIZE;
    }
    let (wanted, number) = STORE.with(|w| (w.intent, w.generation));
    unsafe {
        intent.write(wanted.normalized());
        *generation = number;
    }
    0
}

pub(crate) unsafe extern "C" fn tuning_set(tuning: *const WifiTuning) -> i32 {
    if tuning.is_null() {
        return ERR_ARG;
    }
    let next = unsafe { *tuning }.normalized();
    if let Err(e) = next.validate() {
        return e;
    }
    // the raw UDP6 path (raw receive, driver-less transmit, raw rate) is DMesh's until the `udp6` link is ported
    if next.wants_raw_path() {
        return ERR_UNSUPPORTED;
    }
    let number = STORE.with(|w| {
        if w.tuning == next {
            return None;
        }
        w.tuning = next;
        w.generation = w.generation.wrapping_add(1).max(1);
        Some(w.generation)
    });
    if let Some(number) = number {
        changed(number);
    }
    0
}

pub(crate) unsafe extern "C" fn tuning_get(tuning: *mut WifiTuning) -> i32 {
    if tuning.is_null() {
        return ERR_ARG;
    }
    if unsafe { (*tuning).size } < core::mem::size_of::<WifiTuning>() as u32 {
        return ERR_SIZE;
    }
    let mut t = STORE.with(|w| w.tuning);
    // the switches this owner has report their value in force, DMesh's defaults: A-MPDU on, 802.11b rates off, BSSID check off
    t.ampdu = if t.ampdu == xip_abi::TUNE_OFF { xip_abi::TUNE_OFF } else { xip_abi::TUNE_ON };
    t.rates_11b = if t.rates_11b == xip_abi::TUNE_ON { xip_abi::TUNE_ON } else { xip_abi::TUNE_OFF };
    t.bssid_check = if t.bssid_check == xip_abi::TUNE_ON { xip_abi::TUNE_ON } else { xip_abi::TUNE_OFF };
    unsafe { tuning.write(t.normalized()) };
    0
}

/// The wanted state, for the owner and for the boot code.
pub(crate) fn wanted() -> (WifiIntent, WifiTuning) {
    STORE.with(|w| (w.intent, w.tuning))
}

/// Driver buffer sizes of the tuning (0: the default), `[rx_static, rx_dynamic, tx_dynamic, rx_ba]`.
pub(crate) fn buffers(t: &WifiTuning) -> [u16; 4] {
    [t.rx_static, t.rx_dynamic, t.tx_dynamic, t.rx_ba]
}

/// Set the wanted state before the host starts (the boot credentials), without telling anyone.
#[cfg(target_os = "espidf")]
pub(crate) fn set_boot(intent: WifiIntent) {
    STORE.with(|w| {
        if w.intent != intent {
            w.intent = intent;
            w.generation = w.generation.wrapping_add(1).max(1);
        }
    });
}

/// Reconcile the built-in station owner with the wanted state; runs on the host worker after every change.
#[cfg(target_os = "espidf")]
static mut APPLIED: Option<(WifiIntent, WifiTuning)> = None;

#[cfg(target_os = "espidf")]
pub(crate) unsafe extern "C" fn reconcile() {
    let (intent, tuning) = wanted();
    if let Some((a, t)) = unsafe { *core::ptr::addr_of!(APPLIED) } {
        if a == intent && t == tuning {
            return;
        }
    }
    crate::wifi_esp::stop();
    if intent.radio == 1 && crate::wifi_esp::start(&intent, &tuning) != 0 {
        return;
    }
    unsafe { *core::ptr::addr_of_mut!(APPLIED) = Some((intent, tuning)) };
}

/// The radio around a light sleep (`xip_power.c`): stopped the way an epoch ends; the classic ESP32 releases the driver too, or
/// `esp_light_sleep_start` returns at once. `APPLIED` is forgotten, so `reconcile` brings back the wanted state with the same settings.
#[cfg(target_os = "espidf")]
pub(crate) unsafe extern "C" fn power_suspend() -> bool {
    if !crate::wifi_esp::radio_up() && !crate::wifi_esp::scan_only() {
        return false;
    }
    crate::wifi_esp::stop();
    #[cfg(all(not(target_arch = "riscv32"), not(target_feature = "esp32s3ops")))]
    crate::wifi_esp::restart_driver();
    unsafe { *core::ptr::addr_of_mut!(APPLIED) = None };
    true
}

#[cfg(target_os = "espidf")]
pub(crate) unsafe extern "C" fn power_resume() {
    unsafe { reconcile() };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(name: &[u8]) -> WifiIntent {
        let mut i = WifiIntent::default();
        i.radio = 1;
        i.sta = 1;
        i.sta_ssid[..name.len()].copy_from_slice(name);
        i.sta_ssid_len = name.len() as u8;
        i
    }

    // one store for the process: the cases run in one test so the generations are predictable
    #[test]
    fn the_wanted_state_is_validated_stored_and_numbered() {
        let mut generation = 0;
        let mut back = WifiIntent::default();
        unsafe {
            assert_eq!(get(&mut back, &mut generation), 0);
            assert_eq!((back.radio, generation), (0, 0));
            // a station is accepted and numbered; the same again keeps the number
            assert_eq!(set(&station(b"home"), &mut generation), 0);
            assert_eq!(generation, 1);
            assert_eq!(set(&station(b"home"), &mut generation), 0);
            assert_eq!(generation, 1);
            assert_eq!(get(&mut back, &mut generation), 0);
            assert_eq!(&back.sta_ssid[..4], b"home");
            // what is invalid changes nothing; every combination of the component is available here
            let mut bad = station(b"home");
            bad.channel = 15;
            assert_eq!(set(&bad, &mut generation), ERR_ARG);
            assert_eq!(get(&mut back, &mut generation), 0);
            assert_eq!(generation, 1);
            let mut ap = station(b"home");
            ap.ap = 1;
            assert_eq!(set(&ap, &mut generation), 0);
            assert_eq!(generation, 2);
            let mut now_only = station(b"home");
            now_only.sta = 0;
            assert_eq!(set(&now_only, &mut generation), 0);
            assert_eq!(generation, 3);
            // tuning shares the generation, and refuses what needs a raw path
            let mut t = WifiTuning::default();
            t.ampdu = xip_abi::TUNE_OFF;
            t.rx_static = 12;
            assert_eq!(tuning_set(&t), 0);
            let mut got = WifiTuning::default();
            assert_eq!(tuning_get(&mut got), 0);
            assert_eq!((got.ampdu, got.rates_11b, got.bssid_check, got.rx_static), (xip_abi::TUNE_OFF, xip_abi::TUNE_OFF, xip_abi::TUNE_OFF, 12));
            t.raw_rx = xip_abi::TUNE_OFF;
            assert_eq!(tuning_set(&t), ERR_UNSUPPORTED);
            assert_eq!(get(&mut back, &mut generation), 0);
            assert_eq!(generation, 4);
        }
        assert_eq!(buffers(&wanted().1), [12, 0, 0, 0]);
    }
}
