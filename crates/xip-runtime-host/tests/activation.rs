use std::{cell::Cell, rc::Rc};
use xip_abi::*;
use xip_runtime_host::{quic, NativeTx, Runtime};
#[repr(C)]
struct Owner {
    starts: u32,
    calls: u32,
}
impl Module for Owner {
    fn handle(&mut self, hw: &Hw, c: &CallView) -> i32 {
        if c.is_host(HOST_START) {
            self.starts += 1;
            return 0;
        }
        if c.is_host(HOST_STOP) {
            return 0;
        }
        assert_eq!(self.starts, 1);
        self.calls += 1;
        if c.wants_response() {
            let mut b = [0; 16];
            let mut w = cbor::Writer::new(&mut b);
            w.map(1).uint(1).uint(self.calls as u64);
            hw.message(c.from, c.component, c.method, c.id, w.finish().unwrap());
        }
        0
    }
}
unsafe extern "C" fn entry(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    let hw = Hw::new(host).unwrap();
    let c = CallView::new(call).unwrap();
    let packet = (c.pkt != 0).then(|| Packet::from_raw(hw, c.pkt));
    (&mut *state.cast::<Owner>()).handle_owned(&hw, &c, packet)
}
unsafe extern "C" fn fail(_: *const Host, _: *mut u8, call: *const Call) -> i32 {
    let c = CallView::new(call).unwrap();
    assert!(c.is_host(HOST_START));
    ERR_NOMEM
}
fn packet(component: u64) -> Vec<u8> {
    let mut b = [0; 64];
    let mut w = cbor::Writer::new(&mut b);
    w.map(4)
        .uint(1)
        .uint(component)
        .uint(2)
        .uint(1)
        .uint(3)
        .uint(9)
        .uint(5)
        .map(0);
    let mut p = quic::long_build(quic::VERSION_CBOR, &[1], &[2]).unwrap();
    p.extend(w.finish().unwrap());
    p
}
fn response(rt: &Runtime) -> Vec<u8> {
    let frames = rt.take_link_tx("udp");
    assert_eq!(frames.len(), 1);
    let (_, header) = quic::classify(&frames[0].1, 0);
    frames[0].1[header.body..].to_vec()
}
#[test]
fn first_command_activates_initializes_and_delivers_to_primary_and_alias() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1500, NativeTx::Done);
    rt.set_quic_classifier(true);
    let loads = Rc::new(Cell::new(0));
    let count = loads.clone();
    let loader = rt.clone();
    rt.on_request(move |component| {
        if component != 47 && component != 52 {
            return Err(ERR_NOTFOUND);
        }
        count.set(count.get() + 1);
        let owner = loader.add_handler("disc", entry, 8);
        owner.set_component(47);
        owner.serve_envelope_component(52).unwrap();
        owner.start(&[]);
        Ok(owner)
    });
    for (i, component) in [47, 52, 47].into_iter().enumerate() {
        assert_eq!(rt.inject_frame("udp", 7, -30, &packet(component)), 0);
        rt.run(0);
        let bytes = response(&rt);
        assert_eq!(cbor::get_int(&bytes, 1).unwrap(), Some(component as i64));
        assert_eq!(cbor::get_int(&bytes, 3).unwrap(), Some(9));
        let fields = cbor::get(&bytes, 6).unwrap().unwrap().rest();
        assert_eq!(cbor::get_int(fields, 1).unwrap(), Some(i as i64 + 1));
    }
    assert_eq!(loads.get(), 1);
    assert_eq!(rt.packets_in_use(), 0);
}
#[test]
fn startup_error_is_a_correlated_response_and_the_request_is_not_dispatched() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1500, NativeTx::Done);
    rt.set_quic_classifier(true);
    let loader = rt.clone();
    rt.on_request(move |component| {
        let m = loader.add_handler("broken", fail, 0);
        m.set_component(component);
        m.start(&[]);
        Ok(m)
    });
    assert_eq!(rt.inject_frame("udp", 7, 0, &packet(47)), 0);
    rt.run(0);
    let bytes = response(&rt);
    assert_eq!(cbor::get_int(&bytes, 7).unwrap(), Some(ERR_NOMEM as i64));
    assert_eq!(cbor::get_int(&bytes, 3).unwrap(), Some(9));
    assert_eq!(rt.packets_in_use(), 0);
}
