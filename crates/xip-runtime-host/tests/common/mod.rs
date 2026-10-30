#![allow(dead_code)]
//! Shared helpers of the integration tests: where the host-built modules are.
use std::path::PathBuf;
use xip_runtime_host::{Instance, Runtime};

/// `scripts/build-host-modules.sh` writes the shared objects here (override with XIP_HOST_MODULES).
pub fn module(name: &str) -> PathBuf {
    let dir = std::env::var_os("XIP_HOST_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules"));
    let p = dir.join(format!("{name}.wasm"));
    assert!(p.with_extension("s1.wasm").exists(), "{} missing: run scripts/build-host-modules.sh", p.display());
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

/// The way every request reaches a module now: a 0x444d0002 long packet (CIDs 01 / 02) whose payload is the CBOR envelope, received on a link the
/// classifier treats as QUIC. `Requests` owns that link ("udp") and collects what the runtime sent back, with the long header taken off.
pub struct Requests {
    pub rt: Runtime,
}
impl Requests {
    pub fn new() -> Self {
        let rt = Runtime::default();
        rt.add_native_link("udp", xip_abi::PKT_MAX, xip_runtime_host::NativeTx::Done);
        rt.set_quic_classifier(true);
        Requests { rt }
    }
    pub fn datagram(envelope: &[u8]) -> Vec<u8> {
        let mut v = xip_runtime_host::quic::long_build(xip_runtime_host::quic::VERSION_CBOR, &[1], &[2]).unwrap();
        v.extend_from_slice(envelope);
        v
    }
    /// The envelope of an answer packet: the long header (our CIDs swapped) taken off.
    pub fn unwrap(packet: &[u8]) -> Vec<u8> {
        let (class, h) = xip_runtime_host::quic::classify(packet, 0);
        assert_eq!(class, xip_runtime_host::QuicClass::Long, "an answer is a long packet");
        assert_eq!((h.version, h.dcid.as_slice(), h.scid.as_slice()), (xip_runtime_host::quic::VERSION_CBOR, &[2][..], &[1][..]));
        packet[h.body..].to_vec()
    }
    /// Send one envelope (token 5) and return the envelopes the runtime sent back, in order.
    pub fn send(&self, envelope: &[u8]) -> Vec<Vec<u8>> {
        assert_eq!(self.rt.inject_frame("udp", 5, 0, &Self::datagram(envelope)), 0);
        for _ in 0..4 {
            self.rt.run(0);
        }
        self.rt.take_link_tx("udp").into_iter().map(|(_, f)| Self::unwrap(&f)).collect()
    }
}
