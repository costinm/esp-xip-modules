//! `alloc` for modules, over the host's managed heap (`mem_alloc` / `mem_free`): two ways, the same blocks.
//!
//! * [`HostAlloc`] (feature `allocator-api`, nightly): an explicit `Allocator` value, `Vec::new_in(hw.allocator())`. Nothing
//!   hidden; code that is generic over the allocator can use it.
//! * [`ModuleAlloc`] (feature `global-alloc`): the `#[global_allocator]`, so plain `Vec`, `String`, `Box` and crates written
//!   for `no_std + alloc` work unchanged. A module has no writable statics, so the allocator finds the host through the
//!   thread-pointer register: [`export_module!`](crate::export_module) points it at the host table for the duration of
//!   each call and puts the old value back (a module is flat code and uses no thread-local storage; the register is the
//!   one place the call can leave a pointer that code without a context argument can reach).
//!
//! Every block must be freed before the module stops: the host adds up the bytes and a non-zero balance is `ERR_LEAK`
//! (the slot then needs a reboot to be replaced). A `Vec` that lives in the module state block cannot exist (the state is
//! plain zero-valid data), so hold the raw pointer there and rebuild the `Vec` per call, or free before returning.
//! Blocks are aligned for any type up to 8 bytes (larger alignment fails: null / `AllocError`).

use core::alloc::Layout;

use crate::{Hw, Host};

const ALIGN: usize = 8;

impl Hw {
    fn raw_alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN { return core::ptr::null_mut(); }
        self.mem_alloc(layout.size())
    }
}

#[cfg(feature = "allocator-api")]
mod explicit {
    use super::*;
    use core::alloc::{AllocError, Allocator};
    use core::ptr::NonNull;

    /// An [`Allocator`] over `mem_alloc`/`mem_free`; copy it freely (it is the `Hw` pointer).
    #[derive(Clone, Copy)]
    pub struct HostAlloc(pub Hw);

    impl Hw {
        pub fn allocator(&self) -> HostAlloc { HostAlloc(*self) }
    }

    unsafe impl Allocator for HostAlloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            if layout.size() == 0 {
                // a zero-sized block needs no memory: a dangling, aligned pointer
                return Ok(NonNull::slice_from_raw_parts(unsafe { NonNull::new_unchecked(layout.align() as *mut u8) }, 0));
            }
            let p = NonNull::new(self.0.raw_alloc(layout)).ok_or(AllocError)?;
            Ok(NonNull::slice_from_raw_parts(p, layout.size()))
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            if layout.size() != 0 { self.0.mem_free(ptr.as_ptr()); }
        }
    }
}
#[cfg(feature = "allocator-api")]
pub use explicit::HostAlloc;

// ---- the thread-pointer register: where a call leaves the host table for the global allocator ----

#[cfg(target_arch = "riscv32")]
unsafe fn tp_swap(new: usize) -> usize {
    let old: usize;
    core::arch::asm!("mv {o}, tp", "mv tp, {n}", o = out(reg) old, n = in(reg) new, options(nostack));
    old
}

#[cfg(target_arch = "xtensa")]
unsafe fn tp_swap(new: usize) -> usize {
    let old: usize;
    core::arch::asm!("rur.threadptr {o}", "wur.threadptr {n}", o = out(reg) old, n = in(reg) new, options(nostack));
    old
}

// anything else (host tests): one static, fine for the single-threaded tests
#[cfg(not(any(target_arch = "riscv32", target_arch = "xtensa")))]
unsafe fn tp_swap(new: usize) -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static TP: AtomicUsize = AtomicUsize::new(0);
    TP.swap(new, Ordering::SeqCst)
}

/// Make `host` the table the global allocator uses; the result goes to [`leave`]. [`export_module!`](crate::export_module)
/// does this around every call; a no-op without the `global-alloc` feature.
#[doc(hidden)]
#[inline(always)]
pub unsafe fn enter(host: *const Host) -> usize {
    if cfg!(feature = "global-alloc") { tp_swap(host as usize) } else { 0 }
}

#[doc(hidden)]
#[inline(always)]
pub unsafe fn leave(saved: usize) {
    if cfg!(feature = "global-alloc") { tp_swap(saved); }
}

/// The global allocator: `mem_alloc`/`mem_free` of the host of the call in progress. Outside a call (no host) it fails.
pub struct ModuleAlloc;

unsafe impl core::alloc::GlobalAlloc for ModuleAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let host = tp_swap(0);
        tp_swap(host);
        match Hw::new(host as *const Host) {
            Some(hw) => hw.raw_alloc(layout),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let host = tp_swap(0);
        tp_swap(host);
        if let Some(hw) = Hw::new(host as *const Host) { hw.mem_free(ptr); }
    }
}

#[cfg(all(feature = "global-alloc", not(test)))]
#[global_allocator]
static GLOBAL: ModuleAlloc = ModuleAlloc;

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::ffi::c_void;
    use std::vec::Vec;

    static LIVE: core::sync::atomic::AtomicIsize = core::sync::atomic::AtomicIsize::new(0);

    unsafe extern "C" fn fake_alloc(_: *mut c_void, size: usize) -> *mut c_void {
        LIVE.fetch_add(size as isize, core::sync::atomic::Ordering::SeqCst);
        // keep the size in front, like the C side's header
        let p = std::alloc::alloc(Layout::from_size_align(size + 16, 16).unwrap());
        *(p as *mut usize) = size;
        p.add(16).cast()
    }

    unsafe extern "C" fn fake_free(_: *mut c_void, ptr: *mut c_void) -> core::ffi::c_int {
        let p = (ptr as *mut u8).sub(16);
        let size = *(p as *mut usize);
        LIVE.fetch_sub(size as isize, core::sync::atomic::Ordering::SeqCst);
        std::alloc::dealloc(p, Layout::from_size_align(size + 16, 16).unwrap());
        0
    }

    fn host() -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = 1;
        h.size = core::mem::size_of::<Host>() as u32;
        h.mem_alloc = Some(fake_alloc);
        h.mem_free = Some(fake_free);
        h
    }

    #[test]
    fn the_global_allocator_finds_the_host_of_the_call_and_balances() {
        let h = host();
        let a = ModuleAlloc;
        unsafe {
            let layout = Layout::from_size_align(24, 8).unwrap();
            assert!(core::alloc::GlobalAlloc::alloc(&a, layout).is_null(), "outside a call there is no host");
            let saved = enter(&h);
            let p = core::alloc::GlobalAlloc::alloc(&a, layout);
            assert!(!p.is_null() || !cfg!(feature = "global-alloc"));
            if !p.is_null() {
                assert_eq!(LIVE.load(core::sync::atomic::Ordering::SeqCst), 24);
                core::alloc::GlobalAlloc::dealloc(&a, p, layout);
            }
            assert!(core::alloc::GlobalAlloc::alloc(&a, Layout::from_size_align(8, 16).unwrap()).is_null(), "alignment above 8");
            leave(saved);
            assert_eq!(LIVE.load(core::sync::atomic::Ordering::SeqCst), 0);
        }
        let _: Vec<u8> = Vec::new();
    }

    #[cfg(feature = "allocator-api")]
    #[test]
    fn an_explicit_allocator_balances_a_vec() {
        let h = host();
        let hw = unsafe { Hw::new(&h) }.unwrap();
        {
            let mut v: Vec<u32, _> = Vec::new_in(hw.allocator());
            v.extend(0..100);
            assert_eq!(v.iter().sum::<u32>(), 4950);
            assert!(LIVE.load(core::sync::atomic::Ordering::SeqCst) >= 400);
        }
        assert_eq!(LIVE.load(core::sync::atomic::Ordering::SeqCst), 0);
    }
}
