//! The `wifi` and `wifi-tuning` components of the module ABI (`xip_abi::{Wifi, WifiTuner}`), answered by DMesh's own radio
//! owner: the intended state is the bearer part of the transport profile (`dmesh-wifi-profile` maps it), and the owner
//! (main_runtime) reconciles it as for any other profile change. Modules cannot see the rest of the profile.
use core::sync::atomic::{AtomicU16, Ordering};
use xip_abi::{WifiIntent, WifiTuning, ERR_ARG, ERR_SIZE};
use xip_runtime_esp::WifiOwner;

/// Handed to the module host in `Features`. DMesh reports neither the Wi-Fi state nor a scan through the host.
pub static OWNER: WifiOwner = WifiOwner { set, get, tuning_set, tuning_get, state: None, scan: None, monitor: None };

unsafe extern "C" fn set(intent: *const WifiIntent, generation: *mut u32) -> i32 {
    if intent.is_null() || generation.is_null() { return ERR_ARG; }
    let intent = unsafe { *intent };
    if intent.validate().is_err() { return ERR_ARG; }
    let (ok, number, changed) = crate::profile::STORE.edit(|p| dmesh_wifi_profile::apply_intent(p, &intent).is_some());
    if !ok { return ERR_ARG; }
    unsafe { *generation = number };
    if changed { crate::profile::announce(number); }
    0
}

unsafe extern "C" fn get(intent: *mut WifiIntent, generation: *mut u32) -> i32 {
    if intent.is_null() || generation.is_null() { return ERR_ARG; }
    if unsafe { (*intent).size } < core::mem::size_of::<WifiIntent>() as u32 { return ERR_SIZE; }
    let snap = crate::profile::STORE.snapshot();
    unsafe { intent.write(dmesh_wifi_profile::to_intent(&snap.profile)); *generation = snap.generation };
    0
}

// Driver buffer sizes (0: the default), read when the driver starts (`wifi_esp::wifi_init_config`).
static BUFFERS: [AtomicU16; 4] = [AtomicU16::new(0), AtomicU16::new(0), AtomicU16::new(0), AtomicU16::new(0)];
/// `[rx_static, rx_dynamic, tx_dynamic, rx_ba]` of the `wifi-tuning` component.
pub(crate) fn buffers() -> [u16; 4] {
    [BUFFERS[0].load(Ordering::Acquire), BUFFERS[1].load(Ordering::Acquire), BUFFERS[2].load(Ordering::Acquire), BUFFERS[3].load(Ordering::Acquire)]
}

unsafe extern "C" fn tuning_set(tuning: *const WifiTuning) -> i32 {
    if tuning.is_null() { return ERR_ARG; }
    let t = unsafe { *tuning };
    if t.validate().is_err() { return ERR_ARG; }
    let (ok, number, changed) = crate::profile::STORE.edit(|p| dmesh_wifi_profile::apply_tuning(p, &t).is_some());
    if !ok { return ERR_ARG; }
    let before = buffers();
    for (slot, value) in BUFFERS.iter().zip([t.rx_static, t.rx_dynamic, t.tx_dynamic, t.rx_ba]) { slot.store(value, Ordering::Release); }
    if changed || before != buffers() { crate::profile::announce(number); }
    0
}

unsafe extern "C" fn tuning_get(tuning: *mut WifiTuning) -> i32 {
    if tuning.is_null() { return ERR_ARG; }
    if unsafe { (*tuning).size } < core::mem::size_of::<WifiTuning>() as u32 { return ERR_SIZE; }
    let mut t = dmesh_wifi_profile::to_tuning(&crate::profile::STORE.snapshot().profile);
    [t.rx_static, t.rx_dynamic, t.tx_dynamic, t.rx_ba] = buffers();
    unsafe { tuning.write(t) };
    0
}
