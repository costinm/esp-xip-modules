//! `alloc` for modules, over the host's managed heap (`mem_alloc` / `mem_free`): two ways, the same blocks.
//!
//! * [`HostAlloc`] (feature `allocator-api`, nightly): an explicit `Allocator` value, `Vec::new_in(hw.allocator())`. Nothing
//!   hidden; code that is generic over the allocator can use it.
//! * [`ModuleAlloc`] (feature `global-alloc`): the `#[global_allocator]`, so plain `Vec`, `String`, `Box` and crates written
//!   for `no_std + alloc` work unchanged. Embedded images have one host-pointer cell in the platform's reserved module
//!   RAM arena. The packer encodes its address at build time. [`export_module!`](crate::export_module) installs the Host
//!   pointer for each handler and restores the previous value when it returns. SDK thread pointers and TLS are untouched.
//!
//! Calls sharing an image's allocator cell must be serialized, including future destruction. An allocator context must
//! not escape to another task or interrupt. Keep explicit `HostAlloc` values where an API accepts an allocator.
//! Every block must be freed before the module stops: a non-zero balance is `ERR_LEAK`. Futures and other owned values
//! may persist in instance-owned memory, provided they are dropped inside a handler before the module stops.
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

// ---- the image's current-host cell; never the SDK thread-pointer register ----

#[cfg(all(feature = "global-alloc", any(target_arch = "riscv32", target_arch = "xtensa")))]
mod context {
    // The address is an absolute RAM constant, even for movable PIC flash code.
    // It must match the image header, and the runtime reserves/validates its cell.
    const fn address(text: &str) -> usize {
        let b = text.as_bytes();
        let mut value = 0usize;
        let mut i = if b.len() >= 2 && b[0] == b'0' && b[1] == b'x' { 2 } else { 0 };
        while i < b.len() {
            let digit = match b[i] {
                b'0'..=b'9' => b[i] - b'0',
                b'a'..=b'f' => b[i] - b'a' + 10,
                b'A'..=b'F' => b[i] - b'A' + 10,
                _ => panic!("invalid module RAM address"),
            };
            value = (value << 4) | digit as usize;
            i += 1;
        }
        assert!(value != 0 && value % core::mem::align_of::<usize>() == 0);
        value
    }
    const ADDRESS: usize = address(env!("XIP_MODULE_STATIC_ADDR", "build embedded modules with tools/mkmod.py"));

    pub unsafe fn get() -> usize {
        core::ptr::read_volatile(ADDRESS as *const usize)
    }
    pub unsafe fn swap(new: usize) -> usize {
        let old = get();
        core::ptr::write_volatile(ADDRESS as *mut usize, new);
        old
    }
}

// Native shared modules have ordinary writable storage. The dispatcher serializes
// access to each image; this is not thread-local storage or a thread register.
#[cfg(not(all(feature = "global-alloc", any(target_arch = "riscv32", target_arch = "xtensa"))))]
mod context {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static HOST: AtomicUsize = AtomicUsize::new(0);
    pub unsafe fn get() -> usize { HOST.load(Ordering::SeqCst) }
    pub unsafe fn swap(new: usize) -> usize { HOST.swap(new, Ordering::SeqCst) }
}

/// Install `host` in this image's allocator cell, returning its previous value.
/// `export_module!` does this around every call. Does nothing without `global-alloc`.
#[doc(hidden)]
#[inline(always)]
pub unsafe fn enter(host: *const Host) -> usize {
    if cfg!(feature = "global-alloc") { context::swap(host as usize) } else { 0 }
}

#[doc(hidden)]
#[inline(always)]
pub unsafe fn leave(saved: usize) {
    if cfg!(feature = "global-alloc") { context::swap(saved); }
}

/// The global allocator: `mem_alloc`/`mem_free` of the host of the call in progress. Outside a call (no host) it fails.
pub struct ModuleAlloc;

unsafe impl core::alloc::GlobalAlloc for ModuleAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let host = context::get();
        match Hw::new(host as *const Host) {
            Some(hw) => hw.raw_alloc(layout),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let host = context::get();
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
            if cfg!(feature = "global-alloc") {
                assert_eq!(context::get(), &h as *const Host as usize);
                let nested_host = host();
                let nested = enter(&nested_host);
                assert_eq!(context::get(), &nested_host as *const Host as usize);
                leave(nested);
                assert_eq!(context::get(), &h as *const Host as usize);
            }
            let p = core::alloc::GlobalAlloc::alloc(&a, layout);
            assert!(!p.is_null() || !cfg!(feature = "global-alloc"));
            if !p.is_null() {
                assert_eq!(LIVE.load(core::sync::atomic::Ordering::SeqCst), 24);
                core::alloc::GlobalAlloc::dealloc(&a, p, layout);
            }
            assert!(core::alloc::GlobalAlloc::alloc(&a, Layout::from_size_align(8, 16).unwrap()).is_null(), "alignment above 8");
            leave(saved);
            assert!(core::alloc::GlobalAlloc::alloc(&a, layout).is_null(), "return clears allocator context");
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
