//! The C module of `modules/mod_wasm3`, compiled in by `build.rs`: the wasm3 interpreter as a module, linked into the process and
//! registered as a natively linked handler (`Runtime::add_handler`), like `berry-c`. It is the same source a packed image ships,
//! run on the same ABI; only the cell lives in a static instead of a fixed RAM word.

use std::sync::Mutex;
use xip_abi::{Call, Host};

extern "C" {
    fn xip_handle(host: *const Host, state: *mut u8, call: *const Call) -> i32;
}

/// The entry, serialized: the module keeps its cell in one global because its runtime serves one module call at a time (see `berry-c`).
unsafe extern "C" fn xip_handle_one_at_a_time(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    static CALLS: Mutex<()> = Mutex::new(());
    let _one_call_at_a_time = CALLS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    xip_handle(host, state, call)
}

/// The module entry, as `xip_runtime_host::Runtime::add_handler` takes it (`HandlerFn`).
pub const HANDLER: unsafe extern "C" fn(*const Host, *mut u8, *const Call) -> i32 = xip_handle_one_at_a_time;
