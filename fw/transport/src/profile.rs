//! DMesh policy projection over the desired bearer store.
pub use dmesh_server::firmware_profile::TransportProfile;
use core::sync::atomic::{AtomicUsize, Ordering};
use dmesh_server::firmware_profile::DmeshPolicy;
use dmesh_server::transport_profile::{store::ProfileStore, TransportProfile as Bearers};

fn task_yield() { unsafe { esp_idf_sys::vTaskDelay(1) } }
/// The desired bearer state of the device (the bearer part of `TransportProfile`) and its generation. Owners read it and
/// reconcile; hardware state is theirs.
pub static STORE: ProfileStore = ProfileStore::new(Bearers::new(), task_yield);
// Accessed only inside the store's edit lock. Contains no bearer state.
static mut POLICY: DmeshPolicy = DmeshPolicy::new();
pub(crate) fn with_profile<R>(operation: impl FnOnce(&mut TransportProfile) -> R) -> R {
    with_profile_committed(operation).0
}
pub(crate) fn with_profile_committed<R>(operation: impl FnOnce(&mut TransportProfile) -> R) -> (R, u32, bool) {
    STORE.edit_with_invalidation(|bearers| {
        let mut profile = TransportProfile::with_policy(*bearers, unsafe { POLICY });
        let before = profile.policy();
        let result = operation(&mut profile);
        *bearers = profile.bearers;
        unsafe { POLICY = profile.policy() };
        (result, before != profile.policy())
    })
}
pub(crate) fn snapshot() -> TransportProfile { with_profile(|profile| *profile) }
pub fn generation() -> u32 { STORE.generation() }

static LISTENER: AtomicUsize = AtomicUsize::new(0);
/// Install the owner wake-up callback of a committed change. It must enqueue and return.
pub fn set_listener(listener: Option<fn(u32)>) {
    LISTENER.store(listener.map(|f| f as usize).unwrap_or(0), Ordering::Release);
}
/// A change was committed (by a module through the `wifi` component): wake the owner, which tells the modules.
pub fn announce(generation: u32) {
    let listener = LISTENER.load(Ordering::Acquire);
    if listener != 0 {
        let listener: fn(u32) = unsafe { core::mem::transmute(listener) };
        listener(generation);
    } else {
        notify_modules(generation);
    }
}
/// Tell every module that the wanted Wi-Fi state changed (`HOST_WIFI`).
pub fn notify_modules(generation: u32) {
    unsafe extern "C" { fn xip_host_wifi_changed(generation: u32); }
    unsafe { xip_host_wifi_changed(generation) };
}
