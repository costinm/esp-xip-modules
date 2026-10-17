//! `xip-runtime-host`: the xip runtime for a development host.
//!
//! It loads modules as shared objects (or takes a handler function), gives each an instance with the same event model as
//! the device runtimes (one call at a time, alarms, interrupts, requests, packets), and implements the host table of
//! the host table (`include/xip.h`) in Rust. Time is virtual by default: nothing sleeps, [`Runtime::run`] advances the clock. Peripherals are
//! fixtures: pins can be wired together, an SPI chip select can be a loopback, a scripted device, or a recording replayed
//! later ([`fixtures`]). What a module did is observable: events, log lines, packets sent to the application, the serial
//! output, and an optional trace of every host call ([`Trace`]).
//!
//! Pure Rust. The packet pool, the table of held packets and the memory balance ([`pool`]) follow the semantics of the C
//! runtime's `xip_pool.c`; the contract both implement is `include/xip.h`. There is room for hooks and checks that a device
//! has no space for.
pub mod fixtures;
pub mod pool;
mod hostcalls;
mod runtime;
mod link;
mod wifi;
mod power;

pub use link::NativeTx;
pub use runtime::{Config, Event, Instance, MainMsg, Runtime, Trace, TraceKind};
pub use xip_abi;

/// A module handler: what a module exports as `xip_handle`.
pub type HandlerFn = unsafe extern "C" fn(*const xip_abi::Host, *mut u8, *const xip_abi::Call) -> i32;

/// A module loaded from a shared object built for this host (see `scripts/build-host-modules.sh`).
pub struct Loaded {
    pub handler: HandlerFn,
    _lib: Option<libloading::Library>,
}

impl Loaded {
    /// Open `path` and look up its `xip_handle`.
    ///
    /// Every call copies the file first: each instance gets its own copy of the module's writable statics (the allocator
    /// binding of a Rust module is one), as an image mapped for each instance on a device would.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Loaded, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let copy = std::env::temp_dir().join(format!("xip-host-{}-{}.so", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::copy(path.as_ref(), &copy).map_err(|e| format!("{}: {e}", path.as_ref().display()))?;
        let lib = unsafe { libloading::Library::new(&copy) };
        let _ = std::fs::remove_file(&copy);
        let lib = lib.map_err(|e| format!("{}: {e}", path.as_ref().display()))?;
        let handler = unsafe { *lib.get::<HandlerFn>(b"xip_handle\0").map_err(|e| format!("xip_handle: {e}"))? };
        Ok(Loaded { handler, _lib: Some(lib) })
    }
    /// A handler linked into this process.
    pub fn linked(handler: HandlerFn) -> Loaded {
        Loaded { handler, _lib: None }
    }
}
