//! The `link` component of the host-runtime: frames from a provider (the runtime's driver, or a module like mod_lora) reach the
//! subscribers as packets with `CALL_LINK`; `send` to a link address reaches the provider, and the packet comes back as a TX_DONE
//! event when the sender asked; READY and STATE events; and routes forward frames before any module sees them.
use xip_abi::*;
use xip_runtime_host::{Event, Instance, NativeTx, Runtime};

const C: i32 = 20; // the consumer's component
const FRAME: i32 = 9; // where its subscribed link delivers frames and events
const SEND: i32 = 7; // test commands (from the application)
const SEND_FORGET: i32 = 8;
const ROUTE: i32 = 4;
const TX_DONE: i32 = 11; // where TX_DONE comes back
const P: i32 = 43; // the provider's component
const TX: i32 = 1; // where its link delivers what is sent to it
const INJECT: i32 = 2;
const DONE: i32 = 3;
const READY: i32 = 5;
const STATE: i32 = 6;
const KEEP: i32 = 9; // the provider keeps the next frames it is given

fn link(h: &Host) -> &'static Link {
    unsafe { &*(h.get_component.unwrap()(h.user, c"link".as_ptr()) as *const Link) }
}

/// Event `event`: `from`, `id`, `value`, `flags`, `time_us` (little endian), then the data.
unsafe fn record(h: &Host, event: u16, call: &Call) {
    let mut v = Vec::new();
    v.extend_from_slice(&call.from.to_le_bytes());
    v.extend_from_slice(&call.id.to_le_bytes());
    v.extend_from_slice(&call.value.to_le_bytes());
    v.extend_from_slice(&call.flags.to_le_bytes());
    v.extend_from_slice(&call.time_us.to_le_bytes());
    if !call.data.is_null() {
        v.extend_from_slice(std::slice::from_raw_parts(call.data, call.len));
    }
    (h.emit.unwrap())(h.user, event, VALUE_BYTES as u8, v.as_ptr(), v.len());
}
unsafe fn emit_i32(h: &Host, event: u16, v: i32) {
    (h.emit.unwrap())(h.user, event, VALUE_BYTES as u8, v.to_le_bytes().as_ptr(), 4);
}
unsafe fn packet(h: &Host, data: &[u8]) -> u32 {
    let (mut pkt, mut window, mut room) = (0u32, std::ptr::null_mut(), 0usize);
    assert_eq!((h.pkt_alloc.unwrap())(h.user, &mut pkt), 0);
    assert_eq!((h.pkt_buf.unwrap())(h.user, pkt, &mut window, &mut room), 0);
    std::ptr::copy_nonoverlapping(data.as_ptr(), window, data.len());
    pkt
}

/// state: [link address u32][stashed packet u32][its length u32]
unsafe extern "C" fn consumer(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    let (h, call) = (&*host, &*call);
    let st = state as *mut u32;
    let data = || if call.len == 0 { &[][..] } else { std::slice::from_raw_parts(call.data, call.len) };
    if call.component == HOST_COMPONENT as i32 && call.method == HOST_START as i32 {
        let name = std::ffi::CString::new(data()).unwrap();
        let a = (link(h).open.unwrap())(h.user, name.as_ptr(), C, FRAME);
        assert!(a as u32 >= ADDR_LINK, "open: {a}");
        *st = a as u32;
    } else if call.component == C && call.method == FRAME && call.flags & CALL_LINK_EVENT == 0 {
        assert!(call.flags & CALL_LINK != 0 && call.flags & 1 == 0, "a frame is not a request");
        assert_eq!(call.from, *st);
        record(h, 1, call);
    } else if call.component == C && call.method == FRAME {
        match event_kind(call.flags) {
            LINK_EVENT_READY => {
                record(h, 7, call);
                if *st.add(1) != 0 {
                    let rc = (h.send.unwrap())(h.user, *st.add(1), *st.add(2) as usize, *st, C, TX_DONE, 0x0a0b);
                    emit_i32(h, 3, rc);
                    if rc != ERR_BUSY {
                        *st.add(1) = 0;
                    }
                }
            }
            LINK_EVENT_STATE => record(h, 8, call),
            other => panic!("event {other}"),
        }
    } else if call.component == C && call.method == TX_DONE {
        assert_eq!(call.flags & (CALL_LINK | CALL_LINK_EVENT), CALL_LINK | CALL_LINK_EVENT);
        assert_eq!(event_kind(call.flags), LINK_EVENT_TX_DONE);
        assert_ne!(call.pkt, 0, "the packet comes back");
        record(h, 6, call);
    } else if call.component == C && (call.method == SEND || call.method == SEND_FORGET) {
        let pkt = packet(h, data());
        let (c, m) = if call.method == SEND { (C, TX_DONE) } else { (0, 0) };
        let rc = (h.send.unwrap())(h.user, pkt, call.len, *st, c, m, 0x0a0b);
        emit_i32(h, 3, rc);
        if rc == ERR_BUSY {
            *st.add(1) = pkt; // not consumed: try again on READY
            *st.add(2) = call.len as u32;
        }
    } else if call.component == C && call.method == ROUTE {
        // offset, key length, flags, egress address u32, token u64, key
        let d = data();
        let (offset, klen, flags) = (d[0] as u32, d[1] as usize, d[2] as u32);
        let egress = u32::from_le_bytes(d[3..7].try_into().unwrap());
        let token = u64::from_le_bytes(d[7..15].try_into().unwrap());
        let l = link(h);
        assert_eq!((l.classify.unwrap())(h.user, *st, offset, klen as u32), 0);
        emit_i32(h, 9, (l.route.unwrap())(h.user, *st, d[15..].as_ptr(), klen, egress, token, flags));
    }
    RET_CONTINUE as i32
}

/// state: [link address u32][kept packet u32][keep flag u32]
unsafe extern "C" fn provider(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    let (h, call) = (&*host, &*call);
    let st = state as *mut u32;
    let data = || if call.len == 0 { &[][..] } else { std::slice::from_raw_parts(call.data, call.len) };
    let l = link(h);
    if call.component == HOST_COMPONENT as i32 && call.method == HOST_START as i32 {
        let a = (l.provide.unwrap())(h.user, c"lora".as_ptr(), P, TX, 255);
        emit_i32(h, 4, a);
        *st = a as u32;
        if a > 0 {
            assert_eq!((l.set_state.unwrap())(h.user, a as u32, LINK_UP, 5), 0);
        }
    } else if call.component == P && call.method == TX {
        assert!(call.flags & CALL_LINK != 0);
        record(h, 2, call);
        if *st.add(2) != 0 {
            assert_eq!((h.pkt_keep.unwrap())(h.user, call.pkt), 0);
            *st.add(1) = call.pkt;
        }
    } else if call.component == P && call.method == KEEP {
        *st.add(2) = 1;
    } else if call.component == P && call.method == INJECT {
        let pkt = packet(h, data());
        emit_i32(h, 5, (l.rx.unwrap())(h.user, *st, pkt, call.len, 0x1122, signal(-90, 12), 0));
    } else if call.component == P && call.method == DONE {
        let rc = (l.tx_done.unwrap())(h.user, *st, *st.add(1), data()[0] as i8 as i32);
        emit_i32(h, 10, rc);
        *st.add(1) = 0;
    } else if call.component == P && call.method == READY {
        assert_eq!((l.ready.unwrap())(h.user, *st), 0);
    } else if call.component == P && call.method == STATE {
        assert_eq!((l.set_state.unwrap())(h.user, *st, data()[0] as u32, 0), 0);
    }
    RET_CONTINUE as i32
}

struct Frame {
    from: u32,
    id: u64,
    value: i32,
    flags: u32,
    time_us: u32,
    data: Vec<u8>,
}
fn frame(e: &Event) -> Frame {
    let d = &e.data;
    Frame {
        from: u32::from_le_bytes(d[0..4].try_into().unwrap()),
        id: u64::from_le_bytes(d[4..12].try_into().unwrap()),
        value: i32::from_le_bytes(d[12..16].try_into().unwrap()),
        flags: u32::from_le_bytes(d[16..20].try_into().unwrap()),
        time_us: u32::from_le_bytes(d[20..24].try_into().unwrap()),
        data: d[24..].to_vec(),
    }
}
fn events(m: &Instance, id: u16) -> Vec<Event> {
    m.events().into_iter().filter(|e| e.id == id).collect()
}
fn ints(m: &Instance, id: u16) -> Vec<i32> {
    events(m, id).iter().map(|e| i32::from_le_bytes(e.data[..4].try_into().unwrap())).collect()
}
fn consumer_on(rt: &Runtime, name: &str, component: i32, link: &str) -> Instance {
    let m = rt.add_handler(name, consumer, 16);
    m.set_component(component);
    m.start(link.as_bytes());
    m
}

#[test]
fn frames_of_a_native_link_reach_every_subscriber_stamped_by_the_driver() {
    let rt = Runtime::default();
    let a = consumer_on(&rt, "a", C, "espnow");
    let b = consumer_on(&rt, "b", 21, "espnow");
    rt.run(5); // the driver stamps the frame with the clock at the callback
    let addr = rt.add_native_link("espnow", 250, NativeTx::Done); // after the subscriptions: the order does not matter
    assert_eq!(rt.inject_frame("espnow", mac_token([1, 2, 3, 4, 5, 6]), signal(-47, 0), b"hello"), 0);
    rt.run(0);
    for m in [&a, &b] {
        let e = events(m, 1);
        assert_eq!(e.len(), 1);
        let f = frame(&e[0]);
        assert_eq!((f.from, f.id, rssi(f.value), snr_quarter_db(f.value), f.flags, f.data.as_slice()), (addr, 0x0000_0605_0403_0201, -47, 0, CALL_LINK, &b"hello"[..]));
        assert_eq!(f.time_us, 5000);
    }
    let info = rt.link_info("espnow").unwrap();
    assert_eq!((info.provider, info.state, info.mtu, info.subscribers, info.received), (LINK_NATIVE, LINK_UP, 250, 2, 1));
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn a_sent_frame_comes_back_when_the_link_is_done_with_it() {
    let rt = Runtime::default();
    let a = consumer_on(&rt, "a", C, "espnow");
    rt.add_native_link("espnow", 250, NativeTx::Async);
    // asked for: the driver has it, nothing comes back yet
    assert_eq!(a.send(C, SEND, 0, b"out"), 0);
    rt.run(2);
    assert_eq!(rt.take_link_tx("espnow"), vec![(0x0a0b, b"out".to_vec())]);
    assert!(events(&a, 6).is_empty());
    assert_eq!(rt.packets_in_use(), 1, "the runtime holds the packet while the driver transmits");
    rt.run(3);
    rt.native_tx_done("espnow", 0);
    rt.run(0);
    let e = events(&a, 6);
    assert_eq!(e.len(), 1);
    let f = frame(&e[0]);
    assert_eq!((f.value, f.id, event_kind(f.flags), f.data.as_slice()), (0, 0x0a0b, LINK_EVENT_TX_DONE, &b"out"[..]));
    assert_eq!(f.time_us, 5000, "how long it took");
    // not asked for: the packet is released when the driver is done
    assert_eq!(a.send(C, SEND_FORGET, 0, b"x"), 0);
    rt.run(0);
    rt.native_tx_done("espnow", 0);
    rt.run(0);
    assert_eq!(events(&a, 6).len(), 1);
    // a failure comes back as the outcome
    assert_eq!(a.send(C, SEND, 0, b"y"), 0);
    rt.run(0);
    rt.native_tx_done("espnow", ERR_IO);
    rt.run(0);
    assert_eq!(frame(&events(&a, 6)[1]).value, ERR_IO);
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn a_busy_link_keeps_the_sender_s_packet_until_it_is_ready() {
    let rt = Runtime::default();
    let a = consumer_on(&rt, "a", C, "espnow");
    rt.add_native_link("espnow", 250, NativeTx::Busy);
    assert_eq!(a.send(C, SEND, 0, b"later"), 0);
    rt.run(0);
    assert_eq!(ints(&a, 3), vec![ERR_BUSY]);
    assert_eq!(rt.packets_in_use(), 1, "not consumed: the module still holds it");
    assert!(rt.take_link_tx("espnow").is_empty());
    // room again: the module gets READY and sends the same packet
    rt.set_native_mode("espnow", NativeTx::Done);
    rt.native_ready("espnow");
    rt.run(0);
    assert_eq!(events(&a, 7).len(), 1);
    assert_eq!(ints(&a, 3), vec![ERR_BUSY, 0]);
    assert_eq!(rt.take_link_tx("espnow"), vec![(0x0a0b, b"later".to_vec())]);
    assert_eq!(events(&a, 6).len(), 1);
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn a_module_provides_a_link_like_mod_lora() {
    let rt = Runtime::default();
    let quic = consumer_on(&rt, "quic", C, "lora"); // the subscriber starts first
    let lora = rt.add_handler("lora", provider, 16);
    lora.set_component(P);
    lora.start(b"");
    rt.run(0);
    let addr = ints(&lora, 4)[0] as u32;
    assert!(addr >= ADDR_LINK);
    let info = rt.link_info("lora").unwrap();
    assert_eq!((info.provider, info.state, info.mtu, info.rate_kbps), (LINK_MODULE, LINK_UP, 255, 5));
    assert_eq!(events(&quic, 8).len(), 1, "the subscriber hears the link come up");
    let s = frame(&events(&quic, 8)[0]);
    assert_eq!((s.value, s.id, event_kind(s.flags)), (LINK_UP as i32, 5, LINK_EVENT_STATE));
    // the radio receives
    rt.run(7);
    assert_eq!(lora.send(P, INJECT, 0, b"rx frame"), 0);
    rt.run(0);
    let f = frame(&events(&quic, 1)[0]);
    assert_eq!((f.from, f.id, rssi(f.value), snr_quarter_db(f.value), f.flags, f.data.as_slice()), (addr, 0x1122, -90, 12, CALL_LINK, &b"rx frame"[..]));
    assert_eq!(f.time_us, 7000);
    // the subscriber transmits; a provider that does not keep the frame has sent it when the call returns
    assert_eq!(quic.send(C, SEND, 0, b"tx frame"), 0);
    rt.run(0);
    let f = frame(&events(&lora, 2)[0]);
    assert_eq!((f.from, f.id, f.flags, f.data.as_slice()), (C as u32, 0x0a0b, CALL_LINK, &b"tx frame"[..]));
    let done = frame(&events(&quic, 6)[0]);
    assert_eq!((done.value, done.data.as_slice()), (0, &b"tx frame"[..]));
    // a provider that keeps the frame (the radio transmits) reports it later, with a failure
    assert_eq!(lora.send(P, KEEP, 0, b""), 0);
    assert_eq!(quic.send(C, SEND, 0, b"second"), 0);
    rt.run(0);
    assert_eq!(events(&quic, 6).len(), 1, "not done yet");
    assert_eq!(lora.send(P, DONE, 0, &[(-3i8) as u8]), 0);
    rt.run(0);
    assert_eq!(ints(&lora, 10), vec![0]);
    assert_eq!(frame(&events(&quic, 6)[1]).value, -3);
    // capacity returns and the state changes
    assert_eq!(lora.send(P, READY, 0, b""), 0);
    assert_eq!(lora.send(P, STATE, 0, &[LINK_DOWN as u8]), 0);
    rt.run(0);
    assert_eq!(events(&quic, 7).len(), 1);
    assert_eq!(rt.link_info("lora").unwrap().state, LINK_DOWN);
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn a_provider_that_ends_fails_what_it_was_sending() {
    let rt = Runtime::default();
    let first = rt.add_handler("first", provider, 16);
    let second = rt.add_handler("second", provider, 16);
    first.set_component(P);
    second.set_component(44);
    first.start(b"");
    second.start(b"");
    let quic = consumer_on(&rt, "quic", C, "lora");
    rt.run(0);
    assert_eq!(ints(&second, 4), vec![ERR_BUSY], "a name has one provider");
    assert_eq!(first.send(P, KEEP, 0, b""), 0);
    assert_eq!(quic.send(C, SEND, 0, b"x"), 0);
    rt.run(0);
    assert!(events(&quic, 6).is_empty());
    first.stop();
    rt.run(0);
    assert_eq!(frame(&events(&quic, 6)[0]).value, ERR_IO, "the sender is told");
    assert_eq!(rt.link_info("lora").unwrap().provider, LINK_NONE);
    assert_eq!(frame(events(&quic, 8).last().unwrap()).value, LINK_DOWN as i32);
    // the name stays for its subscribers; nobody provides it: a send finds nobody and the packet is consumed
    assert_eq!(quic.send(C, SEND, 0, b"y"), 0);
    rt.run(0);
    assert_eq!(ints(&quic, 3).last(), Some(&ERR_NOTFOUND));
    assert_eq!(rt.packets_in_use(), 0);
}

fn route_request(offset: u8, key: &[u8], flags: u8, egress: u32, token: u64) -> Vec<u8> {
    let mut v = vec![offset, key.len() as u8, flags];
    v.extend_from_slice(&egress.to_le_bytes());
    v.extend_from_slice(&token.to_le_bytes());
    v.extend_from_slice(key);
    v
}

#[test]
fn a_route_forwards_in_the_runtime_before_any_module_is_woken() {
    let rt = Runtime::default();
    let a = consumer_on(&rt, "a", C, "espnow");
    rt.add_native_link("espnow", 250, NativeTx::Done);
    let udp = rt.add_native_link("udp6", 1200, NativeTx::Done);
    // the connection identifier is the two bytes at offset 1 of a frame
    assert_eq!(a.send(C, ROUTE, 0, &route_request(1, &[0xaa, 0xbb], 0, udp, 77)), 0);
    rt.run(0);
    assert_eq!(ints(&a, 9), vec![0]);
    assert_eq!(rt.inject_frame("espnow", 1, 0, &[0x40, 0xaa, 0xbb, 1, 2, 3]), 0);
    assert_eq!(rt.inject_frame("espnow", 1, 0, &[0x40, 0xaa, 0xcc, 9]), 0); // other identifier: delivered
    assert_eq!(rt.inject_frame("espnow", 1, 0, &[0x40]), 0); // too short for the identifier: delivered
    rt.run(0);
    assert_eq!(rt.take_link_tx("udp6"), vec![(77, vec![0x40, 0xaa, 0xbb, 1, 2, 3])]);
    let got: Vec<Vec<u8>> = events(&a, 1).iter().map(|e| frame(e).data).collect();
    assert_eq!(got, vec![vec![0x40, 0xaa, 0xcc, 9], vec![0x40]]);
    let info = rt.link_info("espnow").unwrap();
    assert_eq!((info.received, info.forwarded, info.dropped), (3, 1, 0));
    assert_eq!(rt.link_info("udp6").unwrap().sent, 1);
    // ROUTE_COPY: forwarded and delivered
    assert_eq!(a.send(C, ROUTE, 0, &route_request(1, &[0xaa, 0xcc], ROUTE_COPY as u8, udp, 78)), 0);
    rt.run(0);
    assert_eq!(rt.inject_frame("espnow", 1, 0, &[0x40, 0xaa, 0xcc, 5]), 0);
    rt.run(0);
    assert_eq!(rt.take_link_tx("udp6").len(), 1);
    assert_eq!(events(&a, 1).len(), 3);
    // an egress that is busy loses the frame and counts it
    rt.set_native_mode("udp6", NativeTx::Busy);
    assert_eq!(rt.inject_frame("espnow", 1, 0, &[0x40, 0xaa, 0xbb, 4]), 0);
    rt.run(0);
    assert_eq!(rt.link_info("espnow").unwrap().dropped, 1);
    assert_eq!(rt.packets_in_use(), 0);
}
