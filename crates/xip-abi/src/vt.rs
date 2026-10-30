//! Component tables for wasm32 modules. A module cannot call into the host through a function pointer, so for a table the
//! host offers (`wifi`, `link`, ...) `get_component` returns a static table of the same layout whose functions are shims:
//! each passes its arguments to the one import `xip.vcall(handle, method, a0..a5)`, and the host runs the call on its native
//! table. The `ctx` argument is dropped (the host has it). Pointer arguments are addresses in the shared memory and must
//! point to pointer-free structs or byte buffers (`modules/ABI-POINTERS.md`). The handle of a component is what `xip.component(name)` returned when the module asked for it: it is the host's to choose and differs between runtimes. The stubs are generated from the
//! table structs by `mesh-api-gen --vtable-conf api/vtables.conf` (`schema.rs`, `vt_shims.rs`).

/// cbindgen:ignore
#[cfg(target_arch = "wasm32")]
pub(crate) mod shim {
    use crate::*;
    use core::ffi::{c_char, c_int, c_void};
    use core::sync::atomic::{AtomicI32, Ordering};

    #[link(wasm_import_module = "xip")]
    extern "C" {
        fn component(name: *const c_char) -> i32;
        fn vcall(component: i32, method: i32, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    }

    pub trait Arg { fn arg(self) -> u64; }
    macro_rules! int_arg { ($($t:ty),*) => { $(impl Arg for $t { fn arg(self) -> u64 { self as u64 } })* } }
    int_arg!(u8, u16, u32, u64, usize, i32, bool);
    impl<T> Arg for *const T { fn arg(self) -> u64 { self as usize as u64 } }
    impl<T> Arg for *mut T { fn arg(self) -> u64 { self as usize as u64 } }

    fn call(c: i32, m: i32, a: [u64; 6]) -> c_int {
        unsafe { vcall(c, m, a[0], a[1], a[2], a[3], a[4], a[5]) }
    }
    /// `shim!(name, handle cell, method, (arg: Type, ...))`: a table function that forwards its arguments to the host.
    macro_rules! shim {
        ($name:ident, $c:expr, $m:expr, ($($a:ident : $t:ty),*)) => {
            unsafe extern "C" fn $name(_ctx: *mut c_void $(, $a: $t)*) -> c_int {
                #[allow(unused_mut)] let mut v = [0u64; 6];
                #[allow(unused)] let mut i = 0;
                $( v[i] = Arg::arg($a); i += 1; )*
                call($c.load(Ordering::Relaxed), $m, v)
            }
        };
    }
    include!("vt_shims.rs");

    /// The table for `name`, or null when the host does not offer it.
    pub fn table(name: &core::ffi::CStr) -> *const c_void {
        let Some((cell, p)) = stub(name.to_bytes()) else { return core::ptr::null() };
        let handle = unsafe { component(name.as_ptr()) };
        if handle <= 0 { return core::ptr::null() }
        cell.store(handle, Ordering::Relaxed);
        p
    }
}
