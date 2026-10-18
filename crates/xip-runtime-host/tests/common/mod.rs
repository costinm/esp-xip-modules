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

/// What a handler does to report something to the application: a message to `ADDR_MAIN` with `id` as the method (what used to be `emit`).
pub unsafe fn notify(h: &xip_abi::Host, id: u16, data: &[u8]) {
    let (mut pkt, mut window, mut room) = (0u32, std::ptr::null_mut(), 0usize);
    assert_eq!((h.pkt_alloc.unwrap())(h.user, &mut pkt), 0);
    assert_eq!((h.pkt_buf.unwrap())(h.user, pkt, &mut window, &mut room), 0);
    std::ptr::copy_nonoverlapping(data.as_ptr(), window, data.len());
    assert_eq!((h.send.unwrap())(h.user, pkt, data.len(), xip_abi::ADDR_MAIN, 0, id as i32, 0), 0);
}

pub fn dump(m: &Instance) {
    for e in m.events() {
        eprintln!("  event 0x{:04x} {}", e.id, e.text());
    }
}
