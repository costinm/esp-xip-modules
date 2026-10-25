//! CBOR console operations, native dispatch and envelope correlation.
mod common;
use common::*;
use xip_abi::cbor::{self, Writer};
use xip_abi::*;
use xip_runtime_host::{Instance, Runtime};

fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 2000];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().expect("fits").len();
    buf.truncate(n);
    buf
}

/// The console module (component 20) has no port: every request is a 0x444d0002 datagram that the runtime parses (the native components Wi-Fi 10,
/// power 11, ip 12 go to it because no module hosts them), and the answer comes back as one.
struct Console { rt: Runtime, q: Requests, c: Instance }
impl Console {
    fn new() -> Self {
        let q = Requests::new();
        let rt = q.rt.clone();
        let c = rt.add_module(module("mod_con"), 32).unwrap();
        c.set_component(20);
        c.start(b"");
        rt.run(0);
        Self { rt, q, c }
    }
    /// Invoke a method with the envelope `{1: component, 2: method, 3: id, 5: fields}`; the answer envelope.
    fn binary(&self, component: u64, method: u64, fields: &[u8]) -> Vec<u8> {
        let mut req = enc(|w| {
            w.map(4).uint(1).uint(component).uint(2).uint(method).uint(3).uint(0x1_0000_004d).uint(5);
        });
        req.extend_from_slice(fields);
        let mut frames = self.q.send(&req);
        assert_eq!(frames.len(), 1, "one answer");
        let answer = frames.remove(0);
        assert_eq!(cbor::get_int(&answer, 1).unwrap(), Some(component as i64));
        assert_eq!(cbor::get_int(&answer, 2).unwrap(), Some(method as i64));
        assert_eq!(cbor::get_int(&answer, 3).unwrap(), Some(0x1_0000_004d));
        answer
    }

    fn binary_ok(&self, component: u64, method: u64, fields: &[u8]) -> Vec<u8> {
        let answer = self.binary(component, method, fields);
        assert!(cbor::get(&answer, 7).unwrap().is_none(), "binary error: {answer:02x?}");
        let mut result = cbor::get(&answer, 6).unwrap().expect("result");
        let rest = result.rest();
        assert!(result.skip());
        rest[..rest.len() - result.rest().len()].to_vec()
    }

    fn binary_error(&self, component: u64, method: u64, fields: &[u8], error: i32) {
        let answer = self.binary(component, method, fields);
        assert_eq!(cbor::get_int(&answer, 7).unwrap(), Some(error as i64));
        assert!(cbor::get(&answer, 6).unwrap().is_none());
    }
}

#[test]
fn native_monitor_and_sleep_use_binary_fields() {
    let k = Console::new();
    let monitor = enc(|w| { w.map(2).uint(1).boolean(true).uint(2).uint(6); });
    k.binary_error(10, 7, &monitor, ERR_UNSUPPORTED);
    let fields = enc(|w| { w.map(1).uint(1).boolean(true); });
    k.binary_ok(10, 2, &fields);
    assert_eq!(k.binary_ok(10, 7, &monitor), [0xa0]);
    assert_eq!(k.rt.wifi_monitor().map(|m| (m.enable, m.channel)), Some((1, 6)));
    k.binary_error(10, 7, &enc(|w| { w.map(1).uint(2).uint(14); }), ERR_ARG);
    k.binary_ok(10, 7, &enc(|w| { w.map(1).uint(1).boolean(false); }));
    let sleep = enc(|w| { w.map(3).uint(1).uint(500).uint(2).uint(3).uint(3).uint(100); });
    assert_eq!(k.binary_ok(11, 1, &sleep), [0xa0]);
    let r = k.rt.power_requests();
    assert_eq!((r.len(), r[0].duration_ms, r[0].cycles, r[0].awake_ms), (1, 500, 3, 100));
    k.binary_error(11, 1, &enc(|w| { w.map(1).uint(1).uint(5); }), ERR_ARG);
    let info = k.binary_ok(11, 2, &[0xa0]);
    assert_eq!(cbor::get_int(&info, 12).unwrap(), Some(3));
    assert_eq!(cbor::get_int(&info, 13).unwrap(), Some(3));
    assert_eq!(k.binary_ok(11, 4, &[0xa0]), [0xa0]);
}

#[test]
fn native_wifi_intent_and_tuning_preserve_layout_and_validation() {
    let k = Console::new();
    let fields = enc(|w| {
        w.map(6).uint(1).boolean(true).uint(2).boolean(true).uint(4).uint(6)
            .uint(6).text("home").uint(7).text("password").uint(10).boolean(true);
    });
    let result = k.binary_ok(10, 2, &fields);
    assert_eq!(cbor::get_int(&result, 1).unwrap(), Some(1));
    let (wanted, generation) = k.rt.wifi_intent();
    assert_eq!((wanted.radio, wanted.sta, wanted.ap, wanted.now, wanted.channel, generation), (1, 1, 0, 1, 6, 1));
    assert_eq!(&wanted.sta_ssid[..wanted.sta_ssid_len as usize], b"home");
    assert_eq!(&wanted.sta_psk[..wanted.sta_psk_len as usize], b"password");
    assert_eq!(wanted.size as usize, std::mem::size_of::<WifiIntent>());
    let got = k.binary_ok(10, 1, &[0xa0]);
    assert_eq!(cbor::get_text(&got, 6).unwrap(), Some(b"home".as_slice()));
    let replacement = enc(|w| { w.map(4).uint(1).boolean(true).uint(3).boolean(true).uint(8).text("lab").uint(5).bytes(&[1,2,3,4,5,6]); });
    k.binary_ok(10, 2, &replacement);
    let (wanted, generation) = k.rt.wifi_intent();
    assert_eq!((wanted.sta, wanted.ap, wanted.now, wanted.channel, generation), (0, 1, 0, 0, 2));
    assert_eq!(wanted.sta_ssid_len, 0);
    assert_eq!(wanted.bssid, [1,2,3,4,5,6]);
    for fields in [
        enc(|w| { w.map(1).uint(4).uint(15); }),
        enc(|w| { w.map(1).uint(6).text("0123456789012345678901234567890123456789"); }),
        enc(|w| { w.map(1).uint(5).bytes(&[1,2]); }),
        enc(|w| { w.map(1).uint(4).uint(300); }),
    ] { k.binary_error(10, 2, &fields, ERR_ARG); }
    assert_eq!(k.rt.wifi_intent().1, 2, "invalid requests must not replace the intent");
    let tuning = enc(|w| { w.map(3).uint(1).uint(2).uint(7).uint(12).uint(11).uint(1); });
    k.binary_ok(10, 6, &tuning);
    assert_eq!((k.rt.wifi_tuning().ampdu, k.rt.wifi_tuning().rx_static, k.rt.wifi_tuning().lr), (TUNE_OFF, 12, TUNE_ON));
    let got = k.binary_ok(10, 5, &[0xa0]);
    assert_eq!(cbor::get_int(&got, 7).unwrap(), Some(12));
    for fields in [enc(|w| { w.map(1).uint(11).uint(3); }), enc(|w| { w.map(1).uint(7).uint(1); })] {
        k.binary_error(10, 6, &fields, ERR_ARG);
    }
}

#[test]
fn flash_auth_nv_defaults_strict_validates_and_clears_development_override() {
    let k = Console::new();
    let get = enc(|w| { w.map(1).uint(1).text("flash.auth"); });
    let read = || {
        let r = k.binary_ok(20, 10, &get);
        match cbor::get(&r, 1).unwrap().unwrap().next().unwrap() {
            cbor::Item::Bytes(v) => v.to_vec(),
            _ => panic!("NV must return bytes"),
        }
    };
    assert_eq!(read(), b"strict");
    for value in [b"unsigned".as_slice(), b"strict"] {
        let set = enc(|w| { w.map(2).uint(1).text("flash.auth").uint(2).bytes(value); });
        k.binary_ok(20, 10, &set);
        assert_eq!(read(), value);
    }
    let bad = enc(|w| { w.map(2).uint(1).text("flash.auth").uint(2).bytes(b"typo"); });
    k.binary_error(20, 10, &bad, ERR_ARG);
    assert_eq!(read(), b"strict");
    let dev = enc(|w| { w.map(2).uint(1).text("flash.auth").uint(2).bytes(b"unsigned"); });
    k.binary_ok(20, 10, &dev);
    let clear = enc(|w| { w.map(2).uint(1).text("flash.auth").uint(2).bytes(b""); });
    k.binary_ok(20, 10, &clear);
    assert_eq!(read(), b"strict");
}

#[test]
fn binary_console_operations_preserve_envelopes_and_return_structured_fields() {
    let k = Console::new();
    let status = k.binary_ok(20, 5, &[0xa0]);
    assert!(matches!(cbor::get(&status, 1).unwrap().unwrap().next(), Some(cbor::Item::Map(_))));
    let heap = k.binary_ok(20, 9, &[0xa0]);
    assert_eq!(cbor::get_int(&heap, 1).unwrap(), Some(41000));
    assert_eq!(cbor::get_int(&heap, 2).unwrap(), Some(30000));
    let start = enc(|w| { w.map(2).uint(1).text("test").uint(2).bytes(b"serve it"); });
    assert_eq!(k.binary_ok(20, 6, &start), [0xa0]);
    assert_eq!(k.rt.state().started, "test");
    assert_eq!(k.rt.state().start_request, "serve it");
    let stop = enc(|w| { w.map(1).uint(1).text("test"); });
    assert_eq!(k.binary_ok(20, 7, &stop), [0xa0]);
    assert_eq!(k.rt.state().stopped, "test");
    let nv_set = enc(|w| { w.map(2).uint(1).text("wifi.rx_static").uint(2).bytes(b"12"); });
    assert_eq!(k.binary_ok(20, 10, &nv_set), [0xa0]);
    let nv_get = enc(|w| { w.map(1).uint(1).text("wifi.rx_static"); });
    let nv = k.binary_ok(20, 10, &nv_get);
    assert_eq!(cbor::get(&nv, 1).unwrap().unwrap().next(), Some(cbor::Item::Bytes(b"12")));
    let bad_start = k.binary(20, 6, &[0xa0]);
    assert_eq!(cbor::get_int(&bad_start, 7).unwrap(), Some(ERR_ARG as i64));
    assert!(cbor::get(&bad_start, 6).unwrap().is_none());
    // fields that are not a map make the datagram a raw frame (the NOW bench's pattern field): the console does not take it and nothing answers
    assert!(k.q.send(&enc(|w| { w.map(4).uint(1).uint(10).uint(2).uint(2).uint(3).uint(1).uint(5).null(); })).is_empty());
    assert_eq!(k.rt.wifi_intent().1, 0, "malformed native fields must not change intent");
    let recursive = k.binary(20, 4, &[0xa0]);
    assert_eq!(cbor::get_int(&recursive, 7).unwrap(), Some(ERR_UNSUPPORTED as i64));
    assert_eq!(k.binary_ok(20, 8, &[0xa0]), [0xa0]);
    assert!(k.rt.state().reset_called);
}

#[test]
fn direct_heap_query_works_without_managed_allocation() {
    let rt = Runtime::new(xip_runtime_host::Config { mem_quota: 1, ..Default::default() });
    let console = rt.add_module(module("mod_con"), 32).unwrap();
    console.set_component(20);
    console.start(b"");
    rt.run(0);
    let heap = console.request(20, 9, &[0xa0]).unwrap();
    assert_eq!(cbor::get_int(&heap, 1).unwrap(), Some(41000));
    assert_eq!(cbor::get_int(&heap, 4).unwrap(), Some(230000));
    for kind in 0..=5 {
        let reply = console.request(20, 9, &[0xa1, 1, kind]).unwrap();
        assert_eq!(reply, &[0xa0]);
    }
    console.stop(); rt.run(0);
    assert_eq!(console.mem_leaked(), 0);
}

#[test]
fn retired_text_methods_and_lines_return_numeric_errors() {
    let k = Console::new();
    for method in 1..=3 {
        let result = k.c.request(20, method, &[0xa0]).unwrap();
        assert_eq!(cbor::Reader::new(&result).next().and_then(|item| item.as_int()), Some(ERR_UNSUPPORTED as i64));
    }
}

#[test]
fn link_send_puts_one_packet_on_the_named_link_and_reports_the_link_errors() {
    let k = Console::new();
    let addr = k.rt.add_native_link("probe", 1500, xip_runtime_host::NativeTx::Done) as u64;
    let payload: Vec<u8> = (0..1400u32).map(|i| (i % 251) as u8).collect();
    let f = enc(|w| { w.map(3).uint(1).uint(addr).uint(2).bytes(&payload).uint(3).uint(0x1234); });
    assert_eq!(k.binary_ok(20, 11, &f), [0xa0]);
    let sent = k.rt.take_link_tx("probe");
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].0, sent[0].1.len(), sent[0].1 == payload), (0x1234, 1400, true));
    // hex text, as the DMesh clients send bytes
    let h = enc(|w| { w.map(2).uint(1).uint(addr).uint(2).text("hex:0102ff"); });
    k.binary_ok(20, 11, &h);
    assert_eq!(k.rt.take_link_tx("probe")[0].1, vec![1, 2, 0xff]);
    // no such link, bad data, empty data
    k.binary_error(20, 11, &enc(|w| { w.map(2).uint(1).uint(0x4000_0fff).uint(2).bytes(b"x"); }), ERR_NOTFOUND);
    k.binary_error(20, 11, &enc(|w| { w.map(2).uint(1).uint(addr).uint(2).text("hex:0"); }), ERR_ARG);
    k.binary_error(20, 11, &enc(|w| { w.map(2).uint(1).uint(addr).uint(2).bytes(b""); }), ERR_ARG);
    k.binary_error(20, 11, &enc(|w| { w.map(1).uint(2).bytes(b"x"); }), ERR_ARG);
    assert_eq!(k.rt.packets_in_use(), 0, "every packet went back to the pool");
}

#[test]
fn wifi_state_and_nonblocking_scan_keep_existing_cbor_shapes() {
    let k = Console::new();
    let fields = enc(|w| { w.map(4).uint(1).boolean(true).uint(2).boolean(true).uint(6).text("test-net").uint(10).boolean(true); });
    k.binary_ok(10, 2, &fields);
    let state = k.binary_ok(10, 3, &[0xa0]);
    assert_eq!(cbor::get_int(&state, 1).unwrap(), Some(2));
    assert_eq!(cbor::get_text(&state, 2).unwrap(), Some(b"test-net".as_slice()));
    assert_eq!(cbor::get_text(&state, 3).unwrap(), Some(b"192.168.49.7".as_slice()));
    assert_eq!(cbor::get_int(&state, 4).unwrap(), Some(-40));
    assert_eq!(cbor::get(&state, 8).unwrap().unwrap().next(), Some(cbor::Item::Bytes(&[2,0,0,0,0,1])));
    assert_eq!(cbor::get(&state, 10).unwrap().unwrap().next(), Some(cbor::Item::Bool(true)));
    k.binary_error(10, 4, &[0xa0], ERR_BUSY);
    let scan = k.binary_ok(10, 4, &[0xa0]);
    let mut rows = cbor::get(&scan, 1).unwrap().unwrap();
    assert_eq!(rows.next(), Some(cbor::Item::Array(2)));
    assert_eq!(rows.next(), Some(cbor::Item::Map(5)));
    assert_eq!(rows.next(), Some(cbor::Item::Uint(1)));
    assert_eq!(rows.next(), Some(cbor::Item::Bytes(b"costin")));
    assert_eq!(rows.next(), Some(cbor::Item::Uint(2)));
    assert!(matches!(rows.next(), Some(cbor::Item::Bytes(mac)) if mac.len()==6));
    assert_eq!(rows.next(), Some(cbor::Item::Uint(3)));
    assert_eq!(rows.next(), Some(cbor::Item::Neg(40)), "RSSI must retain its sign");
}
