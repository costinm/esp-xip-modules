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
mod network;
mod runtime;
mod link;
mod envelope;
mod hosted;
pub mod quic;
mod quic_udp;
mod crypto;
mod native;
mod wifi;
mod power;

pub use link::NativeTx;
pub use quic::{QuicClass, QuicHeader, QuicInfo, QuicKind, QuicPacket, QuicStats};
pub use network::Ports;
pub use quic_udp::{QuicUdp, DEFAULT_PORT as QUIC_UDP_PORT};
pub use runtime::{Config, Event, Instance, MainMsg, Runtime, Trace, TraceKind};
pub use xip_abi;
/// Where the device identity persists (call before the first use; see `xip_psa::set_identity_dir`).
pub use xip_psa::set_identity_dir;
pub use xip_wasm as wasm;
use xip_wasm::SLOTS;

/// A module handler: what a module exports as `xip_handle`.
pub type HandlerFn = unsafe extern "C" fn(*const xip_abi::Host, *mut u8, *const xip_abi::Call) -> i32;

/// A module loaded from a WebAssembly file built for this host (see `scripts/build-host-modules.sh`), or a handler linked into this process.
///
/// A wasm module is linked at the address of its slot (`modules/WASM.md`), so the runtime needs one file per slot: for
/// `mod_rtest.wasm` it reads `mod_rtest.s1.wasm` for the first wasm module of the runtime, `mod_rtest.s2.wasm` for the second, and so on.
#[derive(Clone, serde::Deserialize)]
pub struct ModuleMetadata { pub state_bytes: usize, pub manifest: Vec<u8> }

pub(crate) enum Kind {
    Native(HandlerFn),
    Wasm { stem: std::path::PathBuf },
}

pub struct Loaded {
    pub(crate) kind: Kind,
    metadata: Option<ModuleMetadata>,
}

impl Loaded {
    /// Find the wasm module `path` (`NAME.wasm`, with `NAME.s1.wasm` ... beside it) and its generated metadata `NAME.json`.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Loaded, String> {
        let path = path.as_ref();
        if path.extension().is_none_or(|e| e != "wasm") {
            return Err(format!("{}: modules are WebAssembly files (.wasm)", path.display()));
        }
        let stem = path.with_extension("");
        let first = slot_file(&stem, 1);
        if !first.exists() {
            return Err(format!("{}: missing, run scripts/build-host-modules.sh", first.display()));
        }
        let sidecar = path.with_extension("json");
        let metadata = if sidecar.exists() {
            Some(serde_json::from_slice(&std::fs::read(&sidecar).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?)
        } else { None };
        Ok(Loaded { kind: Kind::Wasm { stem }, metadata })
    }
    /// The same module description (metadata) with `handler`, linked into this process, instead of the wasm code: a test checks the
    /// ABI with native Rust allocations in the place of a module.
    pub fn with_handler(mut self, handler: HandlerFn) -> Loaded {
        self.kind = Kind::Native(handler);
        self
    }
    /// A handler linked into this process.
    pub fn linked(handler: HandlerFn) -> Loaded {
        Loaded { kind: Kind::Native(handler), metadata: None }
    }
}

pub(crate) fn slot_file(stem: &std::path::Path, slot: usize) -> std::path::PathBuf {
    let mut name = stem.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".s{slot}.wasm"));
    stem.with_file_name(name)
}
