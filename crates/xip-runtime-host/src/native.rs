//! `get_component("native")` on the host: the C library functions of `xip_abi::Native` for a handler that is linked into the process (a C
//! module such as `mod_berry`). A wasm module does not get it: its memory is not the process's, so a pointer into it means nothing here.
//! `snprintf` is `None`: Rust cannot define a variadic function, and the process's libc is what a native handler uses anyway.
use std::ffi::{c_char, c_double, c_float, c_int, c_long, c_ulong, c_void};
use xip_abi::{Native, NATIVE_VERSION};

extern "C" {
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn memset(d: *mut c_void, c: c_int, n: usize) -> *mut c_void;
    fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int;
    fn strlen(s: *const c_char) -> usize;
    fn strcmp(a: *const c_char, b: *const c_char) -> c_int;
    fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int;
    fn strcpy(d: *mut c_char, s: *const c_char) -> *mut c_char;
    fn strncpy(d: *mut c_char, s: *const c_char, n: usize) -> *mut c_char;
    fn strcat(d: *mut c_char, s: *const c_char) -> *mut c_char;
    fn strchr(s: *const c_char, c: c_int) -> *mut c_char;
    fn strstr(h: *const c_char, n: *const c_char) -> *mut c_char;
    fn strtod(s: *const c_char, e: *mut *mut c_char) -> c_double;
    fn strtol(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_long;
    fn strtoul(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_ulong;
    fn fmod(x: c_double, y: c_double) -> c_double;
    fn floor(x: c_double) -> c_double;
    fn ceil(x: c_double) -> c_double;
    fn trunc(x: c_double) -> c_double;
    fn rint(x: c_double) -> c_double;
    fn sqrt(x: c_double) -> c_double;
    fn fabs(x: c_double) -> c_double;
    fn copysign(x: c_double, y: c_double) -> c_double;
    fn floorf(x: c_float) -> c_float;
    fn ceilf(x: c_float) -> c_float;
    fn truncf(x: c_float) -> c_float;
    fn rintf(x: c_float) -> c_float;
    fn sqrtf(x: c_float) -> c_float;
    fn fabsf(x: c_float) -> c_float;
    fn copysignf(x: c_float, y: c_float) -> c_float;
}

static NATIVE: Native = Native {
    version: NATIVE_VERSION,
    size: core::mem::size_of::<Native>() as u32,
    memcpy: Some(memcpy),
    memmove: Some(memmove),
    memset: Some(memset),
    memcmp: Some(memcmp),
    strlen: Some(strlen),
    strcmp: Some(strcmp),
    strncmp: Some(strncmp),
    strcpy: Some(strcpy),
    strncpy: Some(strncpy),
    strcat: Some(strcat),
    strchr: Some(strchr),
    strstr: Some(strstr),
    strtod: Some(strtod),
    strtol: Some(strtol),
    strtoul: Some(strtoul),
    snprintf: None,
    fmod: Some(fmod),
    floor: Some(floor),
    ceil: Some(ceil),
    trunc: Some(trunc),
    rint: Some(rint),
    sqrt: Some(sqrt),
    fabs: Some(fabs),
    copysign: Some(copysign),
    floorf: Some(floorf),
    ceilf: Some(ceilf),
    truncf: Some(truncf),
    rintf: Some(rintf),
    sqrtf: Some(sqrtf),
    fabsf: Some(fabsf),
    copysignf: Some(copysignf),
};

pub(crate) fn component() -> *const Native {
    &NATIVE
}
