//! The behaviour of wasm modules is tested through the host runtime (`crates/xip-runtime-host/tests`, which load `mod_rtest`,
//! `mod_system` (with its sys component) and `mod_ble` as wasm). Here: what needs no runtime.
use xip_wasm::WasmSpace;

#[test]
fn rejects_garbage() {
    let space = WasmSpace::new(1 << 20).unwrap();
    assert!(space.load(1, b"not wasm").is_err());
}

#[test]
fn regions_are_inside_the_memory_and_distinct() {
    let space = WasmSpace::new(1 << 20).unwrap();
    let (a, b) = (space.region(100).unwrap(), space.region(100).unwrap());
    assert!(space.contains(a.ptr) && space.contains(b.ptr));
    assert_ne!(a.addr, b.addr);
    assert!(space.region(1 << 30).is_err());
}
