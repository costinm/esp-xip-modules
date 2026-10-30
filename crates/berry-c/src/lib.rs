//! The C module of `modules/mod_berry`, compiled in by `build.rs`: how a C module reaches the host
//! runtime after `modules/WASM.md`. Rust modules come to `xip-runtime-host` as wasm; Berry is C and
//! has no wasm build, so its code is linked into the process instead and registered as a natively
//! linked handler (`Runtime::add_handler`). It is the same source a packed image ships, and the
//! same ABI: `xip_handle(host, state, call)` is exactly the handler's signature, so the module sees
//! the runtime's host table, its state block, and the runtime's accounting (`mem_alloc`, leaks)
//! like any other module. Only the code's location differs: a host build keeps the module cell and
//! Berry's tables in normal memory, a packed image places them at fixed link addresses
//! (`-DXIP_CELL_ADDR`, `modules/common/xip_module.h`).

use std::sync::Mutex;
use xip_abi::{Call, Host};

extern "C" {
    fn xip_handle(host: *const Host, state: *mut u8, call: *const Call) -> i32;

    /// The formatter a packed image carries (`port/xip_libc.c`); it has its own name, so the host
    /// can call it without interposing libc's `snprintf`.
    pub fn xip_fmt_snprintf(s: *mut i8, n: usize, fmt: *const i8, ...) -> i32;
}

/// The entry, serialized: the module keeps its cell in one global because its runtime serves one
/// module call at a time - on a device the runtime and the fixed RAM window give that rule, and a
/// linked build must state it itself. The test binary runs several runtimes, so the lock keeps one
/// call (one instance's cell) live at a time; inside it the C code is exactly the device code.
unsafe extern "C" fn xip_handle_one_at_a_time(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    static CALLS: Mutex<()> = Mutex::new(());
    let _one_call_at_a_time = CALLS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    xip_handle(host, state, call)
}

/// The module entry, as `xip_runtime_host::Runtime::add_handler` takes it (`HandlerFn`).
pub const HANDLER: unsafe extern "C" fn(*const Host, *mut u8, *const Call) -> i32 = xip_handle_one_at_a_time;
