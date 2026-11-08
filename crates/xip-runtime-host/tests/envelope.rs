//! The runtime parses the CBOR envelope `{1: component, 2: method, 3: id, 5: fields}` of a 0x444d0002 datagram (the only place an envelope is parsed) and
//! wraps the module's answer; a module sees an ordinary request (`CALL_ENVELOPE`) and answers with `reply` or a bare negative integer. Module ports
//! deliver raw frames. The native components with no hosting module go to the module of component 49.
mod common;
use common::notify;
use xip_abi::cbor::{self, Writer};
use xip_abi::*;
use xip_runtime_host::{Instance, NativeTx, Runtime};
use common::Requests;

const C: i32 = 60;
const REQ: u16 = 2; // event: has_id, request_id
const SEEN: u16 = 1; // event: component, method, flags, from, token, the data
const M_ECHO: i32 = 1; // answer the fields
const M_FAIL: i32 = 2; // answer a bare error
const M_LATER: i32 = 3; // remember the request, answer it on M_ANSWER
const M_STREAM: i32 = 4; // remember the request, answer with events on M_EVENT, then M_END
const M_ANSWER: i32 = 9;
const M_EVENT: i32 = 10;
const M_END: i32 = 11;

fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 400];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().expect("fits").len();
    buf.truncate(n);
    buf
}

unsafe fn message(h: &Host, dst: u32, component: i32, method: i32, token: u64, data: &[u8]) -> i32 {
    let (mut pkt, mut window, mut room) = (0u32, std::ptr::null_mut(), 0usize);
    assert_eq!((h.pkt_alloc.unwrap())(h.user, &mut pkt), 0);
    assert_eq!((h.pkt_buf.unwrap())(h.user, pkt, &mut window, &mut room), 0);
    std::ptr::copy_nonoverlapping(data.as_ptr(), window, data.len());
    (h.send.unwrap())(h.user, pkt, data.len(), dst, component, method, token)
}

/// state: [from u32][token u64] of the remembered request
unsafe extern "C" fn module(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    let (h, call) = (&*host, &*call);
    if call.component != C || call.flags & CALL_LINK_EVENT != 0 {
        return 0; // not ours, or the TX_DONE of an answer
    }
    let data = if call.len == 0 { &[][..] } else { std::slice::from_raw_parts(call.data, call.len) };
    let mem = state as *mut u64;
    let reply = |bytes: &[u8]| message(h, call.from, call.component, call.method, call.id, bytes);
    if call.method < M_ANSWER {
        let mut seen = Vec::new();
        seen.extend_from_slice(&call.component.to_le_bytes());
        seen.extend_from_slice(&call.method.to_le_bytes());
        seen.extend_from_slice(&call.flags.to_le_bytes());
        seen.extend_from_slice(&call.from.to_le_bytes());
        seen.extend_from_slice(&call.id.to_le_bytes());
        seen.extend_from_slice(data);
        notify(h, SEEN, &seen);
    }
    if call.flags & CALL_ENVELOPE != 0 {
        let mut r = vec![call.request_has_id as u8];
        r.extend_from_slice(&call.request_id.to_le_bytes());
        notify(h, REQ, &r);
    }
    match call.method {
        M_ECHO if call.flags & 1 != 0 => {
            reply(data);
        }
        M_FAIL if call.flags & 1 != 0 => {
            reply(&[0x27]); // -8
        }
        M_LATER | M_STREAM => {
            *mem = call.from as u64;
            *mem.add(1) = call.id;
            *mem.add(2) = call.method as u64;
        }
        M_ANSWER | M_EVENT | M_END => {
            let (from, token, method) = (*mem as u32, *mem.add(1), *mem.add(2) as i32);
            let flag = if call.method == M_EVENT { SEND_MORE } else { 0 };
            message(h, from, C, method | flag, token, &[0xa1, 1, call.value as u8]);
        }
        _ => {}
    }
    0
}

/// The module of component 49 (the sys module, the native fallback): it receives the envelopes of components nobody hosts, as requests for that component.
unsafe extern "C" fn fallback(host: *const Host, _: *mut u8, call: *const Call) -> i32 {
    let (h, call) = (&*host, &*call);
    if call.flags & CALL_ENVELOPE != 0 {
        let mut seen = call.component.to_le_bytes().to_vec();
        seen.extend_from_slice(&call.method.to_le_bytes());
        notify(h, SEEN, &seen);
        message(h, call.from, call.component, call.method, call.id, &[0x21]);
    }
    0
}

struct Bench { q: Requests, m: Instance }
impl Bench {
    fn new() -> Self {
        let q = Requests::new();
        let m = q.rt.add_handler("t", module, 64);
        m.set_component(C);
        m.host_component("t");     // a module is the target of its component's envelopes when it hosts a component (or has a datagram port)
        m.start(b"");
        q.rt.run(0);
        Self { q, m }
    }
    fn send(&self, envelope: &[u8]) -> Vec<Vec<u8>> {
        self.q.send(envelope)
    }
    fn seen(&self) -> Vec<(i32, i32, u32, u32, u64, Vec<u8>)> {
        self.m.events().iter().filter(|e| e.id == SEEN).map(|e| {
            let d = &e.data;
            (i32::from_le_bytes(d[0..4].try_into().unwrap()), i32::from_le_bytes(d[4..8].try_into().unwrap()), u32::from_le_bytes(d[8..12].try_into().unwrap()),
             u32::from_le_bytes(d[12..16].try_into().unwrap()), u64::from_le_bytes(d[16..24].try_into().unwrap()), d[24..].to_vec())
        }).collect()
    }
}

#[test]
fn an_envelope_becomes_an_ordinary_request_and_the_answer_is_wrapped() {
    let b = Bench::new();
    let fields = enc(|w| { w.map(1).uint(1).text("hi"); });
    let mut request = enc(|w| { w.map(4).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64).uint(3).uint(0x1_0000_0001).uint(5); });
    request.extend_from_slice(&fields);
    let frames = b.send(&request);
    // the module saw (component, method, fields) with the request and envelope bits and the reply token, not the envelope
    let seen = b.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].0, seen[0].1, seen[0].2), (C, M_ECHO, 1 | CALL_ENVELOPE));
    assert_eq!(seen[0].4, 5, "the token of the link, not the envelope id");
    assert_eq!(seen[0].5, fields);
    assert_eq!(frames.len(), 1);
    let answer = &frames[0];
    assert_eq!((cbor::get_int(answer, 1).unwrap(), cbor::get_int(answer, 2).unwrap(), cbor::get_int(answer, 3).unwrap()), (Some(C as i64), Some(M_ECHO as i64), Some(0x1_0000_0001)));
    assert!(cbor::get(answer, 7).unwrap().is_none());
    let mut result = cbor::get(answer, 6).unwrap().unwrap();
    assert_eq!(result.next(), Some(cbor::Item::Map(1)));
    // no fields and no id: an empty map for the module, id 0 in the answer
    let frames = b.send(&enc(|w| { w.map(2).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64); }));
    assert_eq!(b.seen()[1].5, [0xa0]);
    assert_eq!(cbor::get_int(&frames[0], 3).unwrap(), Some(0));
    assert_eq!(cbor::get(&frames[0], 6).unwrap().unwrap().next(), Some(cbor::Item::Map(0)));
}

#[test]
fn additional_component_routes_without_fallback_and_preserves_wire_identity() {
    unsafe extern "C" fn owner(host: *const Host, _: *mut u8, call: *const Call) -> i32 {
        if (*call).flags & CALL_ENVELOPE != 0 {
            assert_eq!(((*call).component, (*call).method), (1, 4));
            assert_eq!((*call).request_id, 987);
            message(&*host, (*call).from, 1, 4, (*call).id, &[0xa0]);
        }
        0
    }
    let q=Requests::new();
    let m=q.rt.add_handler("nan",owner,0);m.set_component(46);
    m.serve_envelope_component(1, 1).unwrap();m.start(b"");q.rt.run(0);
    let request=enc(|w| {w.map(4).uint(1).uint(1).uint(2).uint(4).uint(3).uint(987).uint(5).map(1).uint(1).uint(0);});
    let frames=q.send(&request);assert_eq!(frames.len(),1);
    assert_eq!(cbor::get_int(&frames[0],1).unwrap(),Some(1));
    assert_eq!(cbor::get_int(&frames[0],2).unwrap(),Some(4));
    assert_eq!(cbor::get_int(&frames[0],3).unwrap(),Some(987));
    assert!(cbor::get(&frames[0],6).unwrap().is_some());
    let other=q.rt.add_handler("other",owner,0);other.set_component(47);
    assert_eq!(other.serve_envelope_component(1, 1),Err(ERR_BUSY));
    assert_eq!(other.serve_envelope_component(47, 1),Ok(()));assert_eq!(other.serve_envelope_component(47, 1),Err(ERR_ARG));
    assert_eq!(other.serve_envelope_component(0, 1),Err(ERR_ARG));
    m.stop();q.rt.run(0);
    let missing=q.send(&request);assert_eq!(missing.len(),1);assert_eq!(cbor::get_int(&missing[0],7).unwrap(),Some(ERR_NOTFOUND as i64),"stopped owner is unavailable without a loader hook");
    assert_eq!(other.serve_envelope_component(1, 1),Ok(()));
}

#[test]
fn a_bare_error_becomes_the_one_error_shape() {
    let b = Bench::new();
    let frames = b.send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_FAIL as u64).uint(3).uint(77); }));
    assert_eq!(frames.len(), 1);
    assert_eq!(cbor::get_int(&frames[0], 7).unwrap(), Some(-8));
    assert_eq!(cbor::get_int(&frames[0], 3).unwrap(), Some(77));
    assert!(cbor::get(&frames[0], 6).unwrap().is_none());
    assert_eq!(cbor::get_int(&frames[0], 2).unwrap(), Some(M_FAIL as i64));
}

#[test]
fn a_component_nobody_hosts_goes_to_the_module_of_component_49() {
    let b = Bench::new();
    // No owner/loader/fallback: a correlated NOTFOUND, never a wrong-module dispatch.
    let missing=b.send(&enc(|w| { w.map(3).uint(1).uint(61).uint(2).uint(1).uint(3).uint(5); }));
    assert_eq!(missing.len(),1);assert_eq!(cbor::get_int(&missing[0],7).unwrap(),Some(ERR_NOTFOUND as i64));
    assert!(b.seen().is_empty());
    let native = b.q.rt.add_handler("native", fallback, 16);
    native.set_component(49);
    native.start(b"");
    b.q.rt.run(0);
    let frames = b.send(&enc(|w| { w.map(3).uint(1).uint(61).uint(2).uint(3).uint(3).uint(9); }));
    assert_eq!(frames.len(), 1);
    assert_eq!((cbor::get_int(&frames[0], 1).unwrap(), cbor::get_int(&frames[0], 2).unwrap(), cbor::get_int(&frames[0], 3).unwrap(), cbor::get_int(&frames[0], 7).unwrap()), (Some(61), Some(3), Some(9), Some(-2)));
    let seen: Vec<_> = native.events().into_iter().filter(|e| e.id == SEEN).collect();
    assert_eq!((i32::from_le_bytes(seen[0].data[0..4].try_into().unwrap()), i32::from_le_bytes(seen[0].data[4..8].try_into().unwrap())), (61, 3), "the fallback module gets the request for that component");
    assert!(b.seen().is_empty(), "the hosting module of component 60 never saw it");
}

#[test]
fn what_is_not_a_request_envelope_is_never_dispatched() {
    let b = Bench::new();
    // an answer-shaped map and a map without routing fields never reach a module (the classifier counts them)
    assert!(b.send(&enc(|w| { w.map(4).uint(1).uint(C as u64).uint(2).uint(1).uint(3).uint(1).uint(6).map(0); })).is_empty());
    assert!(b.send(&enc(|w| { w.map(1).uint(9).uint(9); })).is_empty());
    assert!(b.send(&enc(|w| { w.map(2).uint(1).uint(C as u64).uint(2).uint(0); })).is_empty());
    assert!(b.seen().is_empty());
}

#[test]
fn module_ports_deliver_raw_frames_whatever_they_hold() {
    let rt = Runtime::default();
    rt.add_native_link("serial", 256, NativeTx::Done);
    let m = rt.add_handler("t", module, 64);
    m.set_component(C);
    assert!(m.bind_port(PORT_CONSOLE) > 0);
    m.start(b"");
    rt.run(0);
    // even a valid envelope on a port is a frame of the link, not a request
    let envelope = enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64).uint(3).uint(1); });
    for record in [&b"plain bytes"[..], &envelope[..]] {
        assert_eq!(rt.inject_frame("serial", 5, 0, record), 0);
    }
    rt.run(0);
    assert!(rt.take_link_tx("serial").is_empty());
    let seen: Vec<_> = m.events().into_iter().filter(|e| e.id == SEEN).collect();
    assert_eq!(seen.len(), 2);
    for e in &seen {
        assert_eq!((u32::from_le_bytes(e.data[8..12].try_into().unwrap()), i32::from_le_bytes(e.data[4..8].try_into().unwrap())), (CALL_LINK, 0));
    }
}

#[test]
fn an_answer_may_come_later_from_another_call_and_keeps_the_request_id() {
    let b = Bench::new();
    assert!(b.send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_LATER as u64).uint(3).uint(500); })).is_empty());
    // another request meanwhile does not take its place
    let other = b.send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_FAIL as u64).uint(3).uint(501); }));
    assert_eq!(cbor::get_int(&other[0], 3).unwrap(), Some(501));
    assert_eq!(b.m.send(C, M_ANSWER, 0, &[]), 0);
    b.q.rt.run(0);
    let frames: Vec<_> = b.q.rt.take_link_tx("udp").into_iter().map(|(_, f)| Requests::unwrap(&f)).collect();
    assert_eq!(frames.len(), 1);
    let answer = &frames[0];
    assert_eq!((cbor::get_int(answer, 3).unwrap(), cbor::get_int(answer, 2).unwrap()), (Some(500), Some(M_LATER as i64)));
    assert!(cbor::get(answer, 6).unwrap().is_some());
}

#[test]
fn send_more_keeps_the_request_open_for_a_subscription() {
    let b = Bench::new();
    assert!(b.send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_STREAM as u64).uint(3).uint(42); })).is_empty());
    for _ in 0..3 {
        assert_eq!(b.m.send(C, M_EVENT, 0, &[]), 0);
    }
    assert_eq!(b.m.send(C, M_END, 0, &[]), 0);
    b.q.rt.run(0);
    let frames: Vec<_> = b.q.rt.take_link_tx("udp").into_iter().map(|(_, f)| Requests::unwrap(&f)).collect();
    assert_eq!(frames.len(), 4);
    for f in &frames {
        assert_eq!((cbor::get_int(f, 3).unwrap(), cbor::get_int(f, 2).unwrap()), (Some(42), Some(M_STREAM as i64)), "every event carries the request id");
        assert!(cbor::get(f, 6).unwrap().is_some());
    }
    // the last one closed the request: the next message is not wrapped any more (and is a plain packet to the peer)
    assert_eq!(b.m.send(C, M_END, 0, &[]), 0);
    b.q.rt.run(0);
    let frames = b.q.rt.take_link_tx("udp");
    assert_eq!(frames.len(), 1);
    assert!(cbor::get(&frames[0].1, 3).unwrap().is_none());
}

#[test]
fn the_module_sees_the_envelope_id_beside_the_reply_token() {
    let b = Bench::new();
    b.send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64).uint(3).uint(0x1_0000_0042); }));
    b.send(&enc(|w| { w.map(2).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64); }));
    let ev: Vec<_> = b.m.events().into_iter().filter(|e| e.id == REQ).collect();
    assert_eq!(ev.len(), 2);
    assert_eq!((ev[0].data[0], u64::from_le_bytes(ev[0].data[1..9].try_into().unwrap())), (1, 0x1_0000_0042));
    assert_eq!(ev[1].data[0], 0, "no id in the envelope");
    assert_eq!(b.seen()[0].4, 5, "the reply token is still the call's id");
}

/// A module (component 46) that forwards an envelope it holds to the runtime's dispatch (`ADDR_ENVELOPE`), as `mod_nan` does for a peer's request, and
/// records what comes back: event 3 = (component, method, id, from, data), event 4 = the `send` result.
unsafe extern "C" fn forwarder(host: *const Host, _: *mut u8, call: *const Call) -> i32 {
    let (h, call) = (&*host, &*call);
    if call.component == 46 && call.method == 1 && call.flags & CALL_LINK == 0 && call.from == ADDR_MAIN {
        let data = std::slice::from_raw_parts(call.data, call.len);
        let rc = message(h, ADDR_ENVELOPE, 0, 0, call.id, data);
        notify(h, 4, &rc.to_le_bytes());
    } else if call.component != 46 && call.component != HOST_COMPONENT as i32 && call.flags & CALL_LINK == 0 && call.from != ADDR_MAIN {
        let mut r = Vec::new();
        for v in [call.component, call.method] { r.extend_from_slice(&v.to_le_bytes()); }
        r.extend_from_slice(&call.id.to_le_bytes());
        r.extend_from_slice(&call.from.to_le_bytes());
        r.extend_from_slice(std::slice::from_raw_parts(call.data, call.len));
        notify(h, 3, &r);
    }
    0
}

#[test]
fn a_module_hands_an_envelope_to_the_runtime_dispatch_and_gets_the_wrapped_answer() {
    let rt = Runtime::default();
    let native = rt.add_handler("native", fallback, 16);
    native.set_component(49);
    let hosting = rt.add_handler("t", module, 64);
    hosting.set_component(C);
    hosting.host_component("t");
    let nan = rt.add_handler("nan", forwarder, 16);
    nan.set_component(46);
    for m in [&native, &hosting, &nan] { m.start(b""); }
    rt.run(0);
    let send = |envelope: &[u8], token: u64| {
        assert_eq!(nan.send(46, 1, token, envelope), 0);
        rt.run(0);
        let rc = i32::from_le_bytes(nan.events().into_iter().filter(|e| e.id == 4).last().unwrap().data[..4].try_into().unwrap());
        rc
    };
    // a native component nobody hosts: the sys module serves it, and the answer is wrapped with the request's id and comes back to the sender with the token
    assert_eq!(send(&enc(|w| { w.map(3).uint(1).uint(10).uint(2).uint(3).uint(3).uint(0x99); }), 7), 0);
    let back: Vec<_> = nan.events().into_iter().filter(|e| e.id == 3).collect();
    assert_eq!(back.len(), 1);
    let d = &back[0].data;
    assert_eq!((i32::from_le_bytes(d[0..4].try_into().unwrap()), i32::from_le_bytes(d[4..8].try_into().unwrap()), u64::from_le_bytes(d[8..16].try_into().unwrap()), u32::from_le_bytes(d[16..20].try_into().unwrap())), (10, 3, 7, 49));
    let answer = &d[20..];
    assert_eq!((cbor::get_int(answer, 1).unwrap(), cbor::get_int(answer, 2).unwrap(), cbor::get_int(answer, 3).unwrap(), cbor::get_int(answer, 7).unwrap()), (Some(10), Some(3), Some(0x99), Some(-2)));
    // a hosting module's own component
    assert_eq!(send(&enc(|w| { w.map(3).uint(1).uint(C as u64).uint(2).uint(M_ECHO as u64).uint(3).uint(5).uint(5).map(0); }), 8), 0);
    let back: Vec<_> = nan.events().into_iter().filter(|e| e.id == 3).collect();
    let answer = &back[1].data[20..];
    assert_eq!((cbor::get_int(answer, 1).unwrap(), cbor::get_int(answer, 3).unwrap()), (Some(C as i64), Some(5)));
    assert!(cbor::get(answer, 6).unwrap().is_some());
    // not an envelope request: refused at the send
    assert_eq!(send(&enc(|w| { w.map(1).uint(9).uint(9); }), 9), ERR_ARG);
}
