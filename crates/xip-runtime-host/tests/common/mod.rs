#![allow(dead_code)]
//! Shared helpers of the integration tests: where the host-built modules are.
use std::path::PathBuf;
use xip_runtime_host::{Instance, Runtime};

/// `scripts/build-host-modules.sh` writes the shared objects here (override with XIP_HOST_MODULES).
pub fn module(name: &str) -> PathBuf {
    let dir = std::env::var_os("XIP_HOST_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules"));
    let p = dir.join(format!("{name}.so"));
    assert!(p.exists(), "{} missing: run scripts/build-host-modules.sh", p.display());
    p
}

pub fn new(name: &str, state: usize) -> (Runtime, Instance) {
    let rt = Runtime::default();
    let m = rt.add_module(module(name), state).expect("load module");
    (rt, m)
}

/// A runtime with one module that was sent the start request.
pub fn started(name: &str, request: &str) -> (Runtime, Instance) {
    let (rt, m) = new(name, 64);
    m.start(request.as_bytes());
    (rt, m)
}

pub fn dump(m: &Instance) {
    for e in m.events() {
        eprintln!("  event 0x{:04x} {}", e.id, e.text());
    }
}
