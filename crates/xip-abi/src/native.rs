//! Native functions: `get_component("native")`, the C library of a module, as a table of the plain C functions of the runtime or the chip's ROM.
//!
//! Unlike every other table the functions have their own C signatures (no `ctx` first): a module calls `memcpy` or `floor` through the pointer
//! exactly as it would call the libc function, and the table is `const` data in the runtime's flash, so a module needs no libc archive and
//! no RAM for it (a module image has no `.data`/`.bss`). The restriction is what makes that possible: a function here neither allocates
//! from a module's heap nor keeps anything about the caller, and touches only the memory it is given. What is wanted from the runtime's own
//! heap or the host's state is not here.
//!
//! The entries are the ROM's (`esp32*.rom.libc.ld`) or the firmware's where the chip has them, and the runtime's own where it does not
//! (`snprintf` formats floats without the C library's float support). The table is never changed after the runtime starts; a missing entry
//! (`None`) is a function the runtime does not offer on this chip. Wasm modules do not use it: their memory is not the runtime's.
use core::ffi::{c_char, c_double, c_float, c_int, c_long, c_ulong, c_void};

pub const NATIVE_VERSION: u32 = 1;

/// `get_component("native")`. Fields are in the order of the C library headers they come from.
#[repr(C)]
pub struct Native {
    /// 1.
    pub version: u32,
    /// Size of this table.
    pub size: u32,
    // ---- string.h ----
    pub memcpy: Option<unsafe extern "C" fn(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void>,
    pub memmove: Option<unsafe extern "C" fn(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void>,
    pub memset: Option<unsafe extern "C" fn(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void>,
    pub memcmp: Option<unsafe extern "C" fn(a: *const c_void, b: *const c_void, n: usize) -> c_int>,
    pub strlen: Option<unsafe extern "C" fn(s: *const c_char) -> usize>,
    pub strcmp: Option<unsafe extern "C" fn(a: *const c_char, b: *const c_char) -> c_int>,
    pub strncmp: Option<unsafe extern "C" fn(a: *const c_char, b: *const c_char, n: usize) -> c_int>,
    pub strcpy: Option<unsafe extern "C" fn(dst: *mut c_char, src: *const c_char) -> *mut c_char>,
    pub strncpy: Option<unsafe extern "C" fn(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char>,
    pub strcat: Option<unsafe extern "C" fn(dst: *mut c_char, src: *const c_char) -> *mut c_char>,
    pub strchr: Option<unsafe extern "C" fn(s: *const c_char, c: c_int) -> *mut c_char>,
    pub strstr: Option<unsafe extern "C" fn(hay: *const c_char, needle: *const c_char) -> *mut c_char>,
    // ---- stdlib.h, stdio.h ----
    pub strtod: Option<unsafe extern "C" fn(s: *const c_char, end: *mut *mut c_char) -> c_double>,
    pub strtol: Option<unsafe extern "C" fn(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long>,
    pub strtoul: Option<unsafe extern "C" fn(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_ulong>,
    /// The runtime's formatter (`%c %d %i %u %o %x %X %s %p %f %e %g %%`, flags, width, precision, `l`/`ll`); `-1` for anything else.
    pub snprintf: Option<unsafe extern "C" fn(out: *mut c_char, n: usize, fmt: *const c_char, ...) -> c_int>,
    // ---- math.h ----
    pub fmod: Option<unsafe extern "C" fn(x: c_double, y: c_double) -> c_double>,
    pub floor: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub ceil: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub trunc: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub rint: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub sqrt: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub fabs: Option<unsafe extern "C" fn(x: c_double) -> c_double>,
    pub copysign: Option<unsafe extern "C" fn(x: c_double, y: c_double) -> c_double>,
    pub floorf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub ceilf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub truncf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub rintf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub sqrtf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub fabsf: Option<unsafe extern "C" fn(x: c_float) -> c_float>,
    pub copysignf: Option<unsafe extern "C" fn(x: c_float, y: c_float) -> c_float>,
}

#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Native>() == 8 + 4 * 31);
