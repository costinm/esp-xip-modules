//! `mod_quic` (and `mod_disc`) as wasm modules of the host runtime: the same modules as the devices.
mod common;
use common::module;
use xip_abi::cbor;
use xip_runtime_host::Runtime;

#[test]
fn quic_module_attaches_and_reports_status() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1448, xip_runtime_host::NativeTx::Done);
    rt.set_quic_classifier(true);
    let m = rt.add_module(module("mod_quic"), 64).unwrap();
    m.set_component(48);
    m.start(b"");
    rt.run(0);
    let status = m.request(48, 1, &[0xa0]).expect("status");
    assert_eq!(cbor::get_int(&status, 1).unwrap(), Some(0), "{status:02x?}");
}

#[test]
fn disc_module_announces_its_record() {
    let rt = Runtime::default();
    let m = rt.add_module(module("mod_disc"), 64).unwrap();
    m.set_component(47);
    m.start(b"");
    rt.run(0);
    let r = m.request(47, 1, &[0xa0]);
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn host_provides_crypto_and_identity_to_native_handlers() {
    let rt = Runtime::default();
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    let host = m.host();
    let get = host.get_component.unwrap();
    for name in [c"crypto", c"identity"] {
        assert!(!unsafe { get(host.user, name.as_ptr()) }.is_null(), "{name:?}");
    }
}

#[test]
fn host_provides_the_native_c_library_to_native_handlers() {
    use xip_abi::{Native, NATIVE_VERSION};
    let rt = Runtime::default();
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    let host = m.host();
    let p = unsafe { (host.get_component.unwrap())(host.user, c"native".as_ptr()) }.cast::<Native>();
    let t = unsafe { p.as_ref() }.expect("native table");
    assert_eq!((t.version, t.size as usize), (NATIVE_VERSION, std::mem::size_of::<Native>()));
    unsafe {
        assert_eq!((t.strlen.unwrap())(c"xip".as_ptr()), 3);
        assert_eq!((t.strcmp.unwrap())(c"a".as_ptr(), c"b".as_ptr()).signum(), -1);
        let mut buf = [0u8; 8];
        (t.memcpy.unwrap())(buf.as_mut_ptr().cast(), b"abc".as_ptr().cast(), 3);
        assert_eq!(&buf[..4], b"abc\0");
        assert_eq!((t.floor.unwrap())(-1.5), -2.0);
        assert_eq!((t.sqrtf.unwrap())(16.0), 4.0);
        assert_eq!((t.strtol.unwrap())(c"-42x".as_ptr(), std::ptr::null_mut(), 10), -42);
    }
    assert!(t.snprintf.is_none(), "variadic: the process libc is used");
}
