//! Desired bearer state. Hardware state and protocol policy belong to owners.
pub use espmod_abi::profile::*;
use core::{cell::UnsafeCell, sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}};

pub struct ProfileStore {
    locked: AtomicBool,
    profile: UnsafeCell<TransportProfile>,
    generation: AtomicU32,
}
// Every access to the cell is serialized by `locked`.
unsafe impl Sync for ProfileStore {}

struct Guard<'a>(&'a AtomicBool);
impl Drop for Guard<'_> {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot { pub profile: TransportProfile, pub generation: u32 }

impl ProfileStore {
    pub const fn new(profile: TransportProfile) -> Self {
        Self { locked: AtomicBool::new(false), profile: UnsafeCell::new(profile), generation: AtomicU32::new(0) }
    }
    fn lock(&self) -> Guard<'_> {
        while self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            #[cfg(target_os = "espidf")]
            unsafe { esp_idf_sys::vTaskDelay(1); }
            #[cfg(not(target_os = "espidf"))]
            core::hint::spin_loop();
        }
        Guard(&self.locked)
    }
    pub fn snapshot(&self) -> Snapshot {
        let _guard = self.lock();
        Snapshot { profile: unsafe { *self.profile.get() }, generation: self.generation() }
    }
    pub fn generation(&self) -> u32 { self.generation.load(Ordering::Acquire) }

    /// Edit and publish atomically. The closure must not call this store again,
    /// do driver work or block. Publication can be announced after a reply is
    /// delivered; generation and profile are committed together here.
    pub fn edit<R>(&self, operation: impl FnOnce(&mut TransportProfile) -> R) -> (R, u32, bool) {
        self.edit_with_invalidation(|profile| (operation(profile), false))
    }
    /// Application policy can share the owner's generation without putting
    /// protocol fields in the generic store. Task context only; no reentry.
    pub fn edit_with_invalidation<R>(&self, operation: impl FnOnce(&mut TransportProfile) -> (R, bool)) -> (R, u32, bool) {
        let _guard = self.lock();
        let before = unsafe { *self.profile.get() };
        let mut profile = before;
        let (result, invalidate) = operation(&mut profile);
        let changed = profile != before || invalidate;
        let generation = if changed {
            unsafe { *self.profile.get() = profile };
            let next = self.generation().wrapping_add(1).max(1);
            self.generation.store(next, Ordering::Release);
            next
        } else { self.generation() };
        (result, generation, changed)
    }
    /// Merge under the same lock as publication. The hook sees this request's
    /// metadata and the latest state. Errors and lease renewals never commit.
    pub fn update_with(&self, request: TransportRequest<'_>, apply: impl FnOnce(TransportRequest<'_>, &mut TransportProfile) -> Result<RequestEffect, i32>) -> Result<(u32, bool), i32> {
        validate_config(request.config).ok_or(espmod_abi::ERR_ARG)?;
        let (result, generation, changed) = self.edit(|p| {
            let before = *p;
            let mut result = apply(request, p);
            if result == Ok(RequestEffect::Applied) && p.config().is_none() { result = Err(espmod_abi::ERR_ARG); }
            if result.is_err() || result == Ok(RequestEffect::LeaseRenewed) { *p = before; }
            result
        });
        result.map(|_| (generation, changed))
    }
    pub fn set(&self, profile: TransportProfile) -> Result<(u32, bool), i32> {
        if profile.requested_transport == Some(TransportKind::Uart) { return Err(espmod_abi::ERR_UNSUPPORTED); }
        profile.config().ok_or(espmod_abi::ERR_ARG)?;
        let (_, generation, changed) = self.edit(|current| *current = profile);
        Ok((generation, changed))
    }
}

pub static STORE: ProfileStore = ProfileStore::new(TransportProfile::new());
type ProductRequestHook = for<'a> fn(TransportRequest<'a>, &mut TransportProfile) -> Result<RequestEffect, i32>;
static REQUEST_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Register the product's request reconciler. Called under the store lock;
/// do not reenter the store or perform blocking driver work.
pub fn set_request_hook(hook: Option<ProductRequestHook>) {
    REQUEST_HOOK.store(hook.map(|h| h as usize).unwrap_or(0), Ordering::Release);
}
pub fn update(request: TransportRequest<'_>) -> Result<u32, i32> {
    let hook = REQUEST_HOOK.load(Ordering::Acquire);
    let (generation, changed) = STORE.update_with(request, |request, p| {
        if hook != 0 {
            let hook: ProductRequestHook = unsafe { core::mem::transmute(hook) };
            hook(request, p)
        } else {
            if request.config.wake_target.is_some() || request.config.iface.is_some() { return Err(espmod_abi::ERR_UNSUPPORTED); }
            apply_request(request, p)
        }
    })?;
    if changed { announce(generation); }
    Ok(generation)
}
pub fn request(bytes: &[u8]) -> Result<u32, i32> {
    update(decode_request(bytes).ok_or(espmod_abi::ERR_ARG)?)
}
static LISTENER: AtomicUsize = AtomicUsize::new(0);

pub fn snapshot() -> TransportProfile { STORE.snapshot().profile }
pub fn snapshot_with_generation() -> Snapshot { STORE.snapshot() }
pub fn generation() -> u32 { STORE.generation() }
pub fn edit<R>(operation: impl FnOnce(&mut TransportProfile) -> R) -> (R, u32, bool) { STORE.edit(operation) }
pub fn set(profile: TransportProfile) -> Result<u32, i32> {
    let (generation, changed) = STORE.set(profile)?;
    if changed { announce(generation); }
    Ok(generation)
}
/// Install the product's owner wake-up callback. It must enqueue and return.
pub fn set_listener(listener: Option<fn(u32)>) {
    LISTENER.store(listener.map(|f| f as usize).unwrap_or(0), Ordering::Release);
}
pub fn announce(generation: u32) {
    let listener = LISTENER.load(Ordering::Acquire);
    if listener != 0 {
        let listener: fn(u32) = unsafe { core::mem::transmute(listener) };
        listener(generation);
    } else { notify_modules(generation); }
}
pub fn notify_modules(generation: u32) {
    #[cfg(target_os = "espidf")]
    unsafe {
        extern "C" { fn espmod_host_transport_changed(generation: u32); }
        espmod_host_transport_changed(generation);
    }
    #[cfg(not(target_os = "espidf"))]
    let _ = generation;
}

#[cfg(target_os = "espidf")]
pub(crate) unsafe extern "C" fn set_hook(bytes: *const u8, len: usize, generation: *mut u32) -> i32 {
    if bytes.is_null() || generation.is_null() { return espmod_abi::ERR_ARG; }
    let bytes = unsafe { core::slice::from_raw_parts(bytes, len) };
    match request(bytes) { Ok(value) => { unsafe { *generation = value }; 0 }, Err(error) => error }
}
#[cfg(target_os = "espidf")]
pub(crate) unsafe extern "C" fn get_hook(profile: *mut espmod_abi::BearerProfile, generation: *mut u32) -> i32 {
    if profile.is_null() || generation.is_null() { return espmod_abi::ERR_ARG; }
    if unsafe { (*profile).size } < core::mem::size_of::<espmod_abi::BearerProfile>() as u32 { return espmod_abi::ERR_SIZE; }
    let snap = snapshot_with_generation();
    let Some(value) = snap.profile.to_abi() else { return espmod_abi::ERR_ARG };
    unsafe { profile.write(value); generation.write(snap.generation); }
    0
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generation_tracks_committed_changes_and_rejects_invalid_profiles() {
        let store = ProfileStore::new(TransportProfile::new());
        let p = TransportProfile::off();
        assert_eq!(store.set(p), Ok((1, true)));
        assert_eq!(store.set(p), Ok((1, false)));
        let mut invalid = p; invalid.ssid_len = 34;
        assert_eq!(store.set(invalid), Err(espmod_abi::ERR_ARG));
        assert_eq!(store.snapshot(), Snapshot { profile: p, generation: 1 });
    }
    #[test]
    fn concurrent_partial_requests_preserve_both_updates() {
        for _ in 0..32 {
            let store = std::sync::Arc::new(ProfileStore::new(TransportProfile::new()));
            let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
            let writer = store.clone(); let other_gate = gate.clone();
            let thread = std::thread::spawn(move || {
                other_gate.wait();
                writer.update_with(TransportRequest { mode: None, config: TransportConfig { ap: Some(0), ..Default::default() } }, apply_request).unwrap();
            });
            gate.wait();
            store.update_with(TransportRequest { mode: None, config: TransportConfig { nan_dw_interval: Some(8), ..Default::default() } }, apply_request).unwrap();
            thread.join().unwrap(); let snap = store.snapshot();
            assert_eq!((snap.profile.ap, snap.profile.nan_dw_interval, snap.generation), (0, 8, 2));
        }
    }
    #[test]
    fn product_metadata_and_lease_do_not_publish() {
        let store = ProfileStore::new(TransportProfile::new()); let before = store.snapshot();
        let mut called = false;
        assert_eq!(store.update_with(TransportRequest { mode: Some(TransportKind::Sta), config: TransportConfig { iface: Some(b"wifi"), wake_target: Some([1;6]), ..Default::default() } }, |request, p| {
            called = true; assert_eq!(request.config.iface, Some(&b"wifi"[..])); assert_eq!(request.config.wake_target, Some([1;6]));
            p.ap = 0; Ok(RequestEffect::LeaseRenewed)
        }), Ok((0, false)));
        assert!(called); assert_eq!(store.snapshot(), before);
        assert_eq!(store.update_with(TransportRequest::default(), |_,p| { p.ap = 0; Err(espmod_abi::ERR_ARG) }), Err(espmod_abi::ERR_ARG));
        assert_eq!(store.snapshot(), before);
    }
    #[test]
    fn panicking_editor_releases_lock_without_publishing() {
        let store = ProfileStore::new(TransportProfile::new());
        let before = store.snapshot();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.edit(|p| { p.ap = 0; panic!("abort edit") })));
        assert_eq!(store.snapshot(), before);
        assert_eq!(store.set(TransportProfile::off()), Ok((1, true)));
    }
    #[test]
    fn concurrent_readers_observe_matching_profile_and_generation() {
        let store = std::sync::Arc::new(ProfileStore::new(TransportProfile::new()));
        let writer = store.clone();
        let thread = std::thread::spawn(move || { for n in 1..=1000 { writer.edit(|p| p.radio_frequency = n); } });
        for _ in 0..1000 { let snap = store.snapshot(); assert_eq!(snap.profile.radio_frequency, snap.generation); }
        thread.join().unwrap();
        assert_eq!(store.snapshot().generation, 1000);
    }
}
