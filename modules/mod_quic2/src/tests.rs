use super::*;
use core::ffi::{c_char, c_int, c_void, CStr};
use links::{tcp_link_profile, LINK_TCP_BASE};
use quic_base::{StaticPacketLease, StaticPacketPool};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, PacketPool, PoolPacket, QuicMux, RxMeta,
    RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId, TxStreamData,
};
use std::collections::{BTreeMap, VecDeque};
use std::{boxed::Box, vec, vec::Vec};
use xip_abi::{
    cbor, Call, Host, PacketPoolInfo, Stream, StreamPacket, ABI_VERSION, ADDR_LINK, CALL_LINK,
    ERR_AGAIN, ERR_ARG, ERR_BUSY, HOST_COMPONENT, HOST_START, HOST_STOP, PKT_HEADROOM, PKT_MAX,
    PKT_SLOT_MIN, SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE, SOCKET_EVENT_WRITE_DONE,
    STREAM_VERSION,
};

struct Slot {
    bytes: Box<[u8]>,
    refs: usize,
    start: usize,
}

#[derive(Default)]
struct FakeConsumer {
    received: Vec<u8>,
    fin: bool,
    feed_limit: Option<usize>,
    pending_out: Option<(u32, u32, bool)>,
    written_calls: usize,
    closed: bool,
}

#[derive(Default)]
struct FakeSocket {
    rx_bytes: VecDeque<u8>,
    tx_inflight: Option<(u32, usize, usize)>,
    tx_completed: Vec<Vec<u8>>,
    closed: bool,
}

#[derive(Default)]
struct FakeHost {
    next_pkt: u32,
    held: BTreeMap<u32, Slot>,
    link_tx: Vec<(u32, u64, Vec<u8>)>,
    sockets: BTreeMap<u32, FakeSocket>,
    consumers: BTreeMap<u32, FakeConsumer>,
    next_stream: u32,
    feed_limit_default: Option<usize>,
    allocations: BTreeMap<usize, core::alloc::Layout>,
    alarms: BTreeMap<i32, u32>,
    next_alarm: i32,
    replies: Vec<Vec<u8>>,
    now_us: u64,
}

unsafe fn fh<'a>(u: *mut c_void) -> &'a mut FakeHost {
    &mut *u.cast::<FakeHost>()
}

impl FakeHost {
    fn alloc_pkt(&mut self) -> u32 {
        self.next_pkt += 1;
        let h = self.next_pkt;
        self.held.insert(
            h,
            Slot {
                bytes: vec![0u8; PKT_SLOT_MIN as usize].into_boxed_slice(),
                refs: 1,
                start: PKT_HEADROOM as usize,
            },
        );
        h
    }

    fn release_pkt(&mut self, h: u32) -> i32 {
        let Some(slot) = self.held.get_mut(&h) else {
            return ERR_ARG;
        };
        slot.refs -= 1;
        if slot.refs == 0 {
            self.held.remove(&h);
        }
        0
    }

    fn complete_tcp_tx(&mut self, socket: u32) -> Option<Vec<u8>> {
        let (pkt, start, len) = self.sockets.get_mut(&socket)?.tx_inflight.take()?;
        let data = self.held[&pkt].bytes[start..start + len].to_vec();
        self.release_pkt(pkt);
        self.sockets.get_mut(&socket).unwrap().tx_completed.push(data.clone());
        Some(data)
    }

    fn queue_consumer_reply(&mut self, stream_handle: u32, payload: &[u8], fin: bool) {
        let pkt = self.alloc_pkt();
        let start = PKT_HEADROOM as usize;
        self.held.get_mut(&pkt).unwrap().bytes[start..start + payload.len()]
            .copy_from_slice(payload);
        let c = self.consumers.get_mut(&stream_handle).unwrap();
        assert!(c.pending_out.is_none());
        c.pending_out = Some((pkt, payload.len() as u32, fin));
    }
}

unsafe extern "C" fn h_now_ms(u: *mut c_void) -> u64 {
    fh(u).now_us / 1000
}
unsafe extern "C" fn h_now_us(u: *mut c_void) -> u64 {
    fh(u).now_us
}
unsafe extern "C" fn h_mem_alloc(u: *mut c_void, size: usize) -> *mut c_void {
    let layout = core::alloc::Layout::from_size_align(size.max(1), 16).unwrap();
    let ptr = std::alloc::alloc_zeroed(layout);
    if !ptr.is_null() {
        fh(u).allocations.insert(ptr as usize, layout);
    }
    ptr.cast()
}
unsafe extern "C" fn h_mem_free(u: *mut c_void, ptr: *mut c_void) -> c_int {
    if let Some(layout) = fh(u).allocations.remove(&(ptr as usize)) {
        std::alloc::dealloc(ptr.cast(), layout);
        0
    } else {
        ERR_ARG
    }
}
unsafe extern "C" fn h_pkt_info(_u: *mut c_void, headroom: *mut u32, max: *mut u32) -> c_int {
    *headroom = PKT_HEADROOM;
    *max = PKT_MAX;
    0
}
unsafe extern "C" fn h_pkt_pool_info(u: *mut c_void, out: *mut PacketPoolInfo) -> c_int {
    let f = fh(u);
    *out = PacketPoolInfo {
        size: core::mem::size_of::<PacketPoolInfo>() as u32,
        slot_bytes: PKT_SLOT_MIN,
        headroom: PKT_HEADROOM,
        max_packet: PKT_MAX,
        capacity: 64,
        available: 64u32.saturating_sub(f.held.len() as u32),
    };
    0
}
unsafe extern "C" fn h_pkt_alloc(u: *mut c_void, out: *mut u32) -> c_int {
    *out = fh(u).alloc_pkt();
    0
}
unsafe extern "C" fn h_pkt_buf(
    u: *mut c_void,
    pkt: u32,
    ptr: *mut *mut u8,
    max: *mut usize,
) -> c_int {
    let Some(s) = fh(u).held.get_mut(&pkt) else {
        return ERR_ARG;
    };
    if s.refs != 1 {
        return ERR_BUSY;
    }
    *ptr = s.bytes.as_mut_ptr().add(s.start);
    *max = PKT_MAX as usize;
    0
}
unsafe extern "C" fn h_pkt_span(
    u: *mut c_void,
    pkt: u32,
    base: *mut *mut u8,
    cap: *mut usize,
) -> c_int {
    let Some(s) = fh(u).held.get_mut(&pkt) else {
        return ERR_ARG;
    };
    *base = s.bytes.as_mut_ptr();
    *cap = s.bytes.len();
    0
}
unsafe extern "C" fn h_pkt_keep(u: *mut c_void, pkt: u32) -> c_int {
    let Some(s) = fh(u).held.get_mut(&pkt) else {
        return ERR_ARG;
    };
    s.refs += 1;
    0
}
unsafe extern "C" fn h_pkt_free(u: *mut c_void, pkt: u32) -> c_int {
    fh(u).release_pkt(pkt)
}
unsafe extern "C" fn h_pkt_tx(
    u: *mut c_void,
    pkt: u32,
    start: u32,
    len: u32,
    link: u32,
    peer: u64,
) -> c_int {
    let f = fh(u);
    if !f.held.contains_key(&pkt) {
        return ERR_ARG;
    }
    let data = f.held[&pkt].bytes[start as usize..(start + len) as usize].to_vec();
    f.link_tx.push((link, peer, data));
    f.release_pkt(pkt);
    0
}
unsafe extern "C" fn h_send(
    u: *mut c_void,
    pkt: u32,
    len: usize,
    dst: u32,
    _component: c_int,
    _method: c_int,
    id: u64,
) -> c_int {
    let f = fh(u);
    let Some(s) = f.held.get(&pkt) else {
        return ERR_ARG;
    };
    if dst >= ADDR_LINK {
        let data = s.bytes[s.start..s.start + len].to_vec();
        f.link_tx.push((dst, id, data));
        f.release_pkt(pkt);
        return 0;
    }
    let data = s.bytes[s.start..s.start + len].to_vec();
    f.replies.push(data);
    f.release_pkt(pkt);
    0
}
unsafe extern "C" fn h_socket_read(
    u: *mut c_void,
    socket: u32,
    out: *mut u8,
    len: usize,
) -> c_int {
    let Some(sock) = fh(u).sockets.get_mut(&socket) else {
        return ERR_ARG;
    };
    if sock.closed {
        return 0;
    }
    if sock.rx_bytes.is_empty() {
        return ERR_AGAIN;
    }
    let n = len.min(sock.rx_bytes.len());
    for i in 0..n {
        *out.add(i) = sock.rx_bytes.pop_front().unwrap();
    }
    n as c_int
}
unsafe extern "C" fn h_socket_close(u: *mut c_void, socket: u32) -> c_int {
    let f = fh(u);
    let Some(mut sock) = f.sockets.remove(&socket) else {
        return ERR_ARG;
    };
    sock.closed = true;
    if let Some((pkt, _, _)) = sock.tx_inflight.take() {
        f.release_pkt(pkt);
    }
    0
}
unsafe extern "C" fn h_socket_write_pkt(
    u: *mut c_void,
    socket: u32,
    pkt: u32,
    start: u32,
    len: u32,
) -> c_int {
    let f = fh(u);
    let Some(s) = f.held.get(&pkt) else {
        return ERR_ARG;
    };
    if s.refs != 1 {
        return ERR_BUSY;
    }
    let Some(sock) = f.sockets.get_mut(&socket) else {
        return ERR_ARG;
    };
    if sock.tx_inflight.is_some() {
        return ERR_BUSY;
    }
    sock.tx_inflight = Some((pkt, start as usize, len as usize));
    0
}
unsafe extern "C" fn h_alarm_set(
    u: *mut c_void,
    delay: u32,
    _window: u32,
    _interval: u32,
    _flags: u32,
    _c: c_int,
    _m: c_int,
) -> c_int {
    let f = fh(u);
    let id = f.next_alarm;
    f.next_alarm += 1;
    f.alarms.insert(id, delay);
    id
}
unsafe extern "C" fn h_alarm_cancel(u: *mut c_void, id: c_int) -> c_int {
    fh(u).alarms.remove(&id);
    0
}

unsafe extern "C" fn s_open(u: *mut c_void, _env: *const u8, _len: usize) -> c_int {
    let f = fh(u);
    f.next_stream += 1;
    let id = f.next_stream;
    f.consumers.insert(
        id,
        FakeConsumer {
            feed_limit: f.feed_limit_default,
            ..Default::default()
        },
    );
    id as c_int
}
unsafe extern "C" fn s_feed(
    u: *mut c_void,
    handle: u32,
    data: *const u8,
    len: usize,
    fin: u32,
) -> c_int {
    let Some(c) = fh(u).consumers.get_mut(&handle) else {
        return xip_abi::ERR_NOTFOUND;
    };
    if fin != 0 {
        assert_eq!(len, 0);
        c.fin = true;
        return 0;
    }
    let cap = c.feed_limit.unwrap_or(usize::MAX);
    let take = len.min(cap);
    let slice = core::slice::from_raw_parts(data, take);
    c.received.extend_from_slice(slice);
    take as c_int
}
unsafe extern "C" fn s_close(u: *mut c_void, handle: u32) -> c_int {
    let f = fh(u);
    if let Some(c) = f.consumers.get_mut(&handle) {
        c.closed = true;
        if let Some((pkt, _, _)) = c.pending_out.take() {
            f.release_pkt(pkt);
        }
        0
    } else {
        xip_abi::ERR_NOTFOUND
    }
}
unsafe extern "C" fn s_take(u: *mut c_void, handle: u32, out: *mut StreamPacket) -> c_int {
    let Some(c) = fh(u).consumers.get_mut(&handle) else {
        return xip_abi::ERR_NOTFOUND;
    };
    if c.closed {
        return xip_abi::ERR_NOTFOUND;
    }
    let Some((pkt, len, fin)) = c.pending_out.take() else {
        return 0;
    };
    *out = StreamPacket {
        packet: pkt,
        len,
        fin: u32::from(fin),
    };
    1
}
unsafe extern "C" fn s_written(u: *mut c_void, handle: u32) -> c_int {
    let Some(c) = fh(u).consumers.get_mut(&handle) else {
        return xip_abi::ERR_NOTFOUND;
    };
    c.written_calls += 1;
    0
}

static STREAM_TABLE: Stream = Stream {
    version: STREAM_VERSION,
    size: core::mem::size_of::<Stream>() as u32,
    listen: None,
    open: Some(s_open),
    feed: Some(s_feed),
    close: Some(s_close),
    write_pkt: None,
    take: Some(s_take),
    written: Some(s_written),
};

unsafe extern "C" fn h_get_component(_u: *mut c_void, name: *const c_char) -> *const c_void {
    if CStr::from_ptr(name).to_bytes() == b"stream" {
        (&STREAM_TABLE as *const Stream).cast()
    } else {
        core::ptr::null()
    }
}

fn make_host(fake: &mut FakeHost) -> Host {
    let mut h: Host = unsafe { core::mem::zeroed() };
    h.abi_version = ABI_VERSION;
    h.size = core::mem::size_of::<Host>() as u32;
    h.user = (fake as *mut FakeHost).cast();
    h.now_ms = Some(h_now_ms);
    h.now_us = Some(h_now_us);
    h.mem_alloc = Some(h_mem_alloc);
    h.mem_free = Some(h_mem_free);
    h.pkt_info = Some(h_pkt_info);
    h.pkt_pool_info = Some(h_pkt_pool_info);
    h.pkt_alloc = Some(h_pkt_alloc);
    h.pkt_buf = Some(h_pkt_buf);
    h.pkt_span = Some(h_pkt_span);
    h.pkt_keep = Some(h_pkt_keep);
    h.pkt_free = Some(h_pkt_free);
    h.pkt_tx = Some(h_pkt_tx);
    h.send = Some(h_send);
    h.socket_read = Some(h_socket_read);
    h.socket_close = Some(h_socket_close);
    h.socket_write_pkt = Some(h_socket_write_pkt);
    h.alarm_set = Some(h_alarm_set);
    h.alarm_cancel = Some(h_alarm_cancel);
    h.get_component = Some(h_get_component);
    h
}

fn dispatch(host: &Host, state: &mut State, call: &Call) -> i32 {
    let hw = unsafe { Hw::new(host) }.unwrap();
    let view = CallView {
        component: call.component,
        method: call.method,
        value: call.value,
        flags: call.flags,
        data: if call.data.is_null() || call.len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(call.data, call.len) }
        },
        from: call.from,
        id: call.id,
        pkt: call.pkt,
        time_us: call.time_us,
        request_id: None,
        src: None,
    };
    let pkt = if call.pkt != 0 {
        Some(unsafe { Packet::from_raw(hw, call.pkt) })
    } else {
        None
    };
    state.handle_owned(&hw, &view, pkt)
}

fn call_host(state: &mut State, host: &Host, method: u32) -> i32 {
    let c = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: HOST_COMPONENT as i32,
        method: method as i32,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(host, state, &c)
}

fn call_link(
    state: &mut State,
    host: &Host,
    fake: &mut FakeHost,
    method: i32,
    from: u32,
    token: u64,
    payload: &[u8],
) -> i32 {
    let pkt = fake.alloc_pkt();
    let start = PKT_HEADROOM as usize;
    let data_ptr = {
        let s = fake.held.get_mut(&pkt).unwrap();
        s.bytes[start..start + payload.len()].copy_from_slice(payload);
        s.bytes[start..].as_ptr()
    };
    let c = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method,
        flags: CALL_LINK,
        data: data_ptr,
        len: payload.len(),
        pkt,
        from,
        id: token,
        time_us: fake.now_us as u32,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(host, state, &c)
}

fn call_socket(state: &mut State, host: &Host, socket: u32, event: u32) -> i32 {
    let c = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 0,
        id: u64::from(socket),
        value: event as i32,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(host, state, &c)
}

#[derive(Default)]
struct ClientRx {
    streams: BTreeMap<StreamId, (Vec<u8>, bool)>,
}

impl<P: PoolPacket> MuxCallbacks<P> for ClientRx {
    fn on_stream_data(
        &mut self,
        _assoc: AssociationId,
        stream_id: StreamId,
        _offset: u64,
        data: RxStreamChunk<'_, P>,
        fin: bool,
    ) -> Result<RxOutcome<P>, StreamError> {
        let entry = self.streams.entry(stream_id).or_default();
        entry.0.extend_from_slice(data.bytes());
        if fin {
            entry.1 = true;
        }
        Ok(RxOutcome::Consumed(data.len()))
    }
    fn on_stream_writable(&mut self, _: AssociationId, _: StreamId, _: usize) {}
    fn on_association_writable(&mut self, _: AssociationId, _: u64) {}
    fn on_device_writable(&mut self) {}
    fn on_stream_closed(&mut self, _: AssociationId, _: StreamId, _: StreamCloseReason) {}
    fn on_association_broken(&mut self, _: AssociationId, _: AssociationBrokenReason) {}
}

#[test]
fn quic2_multiplexed_streams_across_serial_and_tcp() {
    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let mut state = State::default();

    assert_eq!(call_host(&mut state, &host, HOST_START), 0);

    let pool: StaticPacketPool<32> = StaticPacketPool::new();
    let mut client_mux = QuicMux::new(&pool, embedded_mux_limits());
    client_mux.add_association(OWNER_ASSOC, true).unwrap();
    let mut client_assoc: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(default_assoc_config());
    client_assoc.upsert_link(LINK_SERIAL, serial_link_profile());

    // 1. Open Stream 0 over PPP serial.
    let s0 = client_mux.open_stream(OWNER_ASSOC, true).unwrap();
    client_mux
        .write_stream(
            OWNER_ASSOC,
            s0,
            TxStreamData::Bytes(b"serial-quic2-req"),
            true,
            &mut client_assoc,
        )
        .unwrap();
    let out0 = client_assoc.pop_outbound_packet().unwrap();
    assert_eq!(
        call_link(
            &mut state,
            &host,
            &mut fake,
            0,
            ADDR_LINK + 1,
            QUIC2_SERIAL_PORT,
            out0.packet.bytes()
        ),
        0
    );
    assert_eq!(fake.consumers[&1].received, b"serial-quic2-req");
    assert!(fake.consumers[&1].fin);

    fake.queue_consumer_reply(1, b"serial-quic2-resp", true);
    let tick_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: TICK,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(&host, &mut state, &tick_call);

    let mut client_rx = ClientRx::default();
    for (_link, _peer, wire) in core::mem::take(&mut fake.link_tx) {
        let mut p = (&pool).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = client_assoc.receive_packet(LINK_SERIAL, p, 2_000).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            client_mux
                .receive_packet(
                    RxMeta {
                        association: OWNER_ASSOC,
                        received_at_us: 2_000,
                    },
                    mux_pkt,
                    &mut client_assoc,
                    &mut client_rx,
                )
                .unwrap();
        }
    }
    assert_eq!(client_rx.streams[&s0].0, b"serial-quic2-resp");
    assert!(client_rx.streams[&s0].1);

    // 2. Open a TCP socket on `--stream 6` (`sock_id = 77`) and send Stream 4 over TCP!
    let sock_id = 77u32;
    fake.sockets.insert(sock_id, FakeSocket::default());
    assert_eq!(call_socket(&mut state, &host, sock_id, SOCKET_EVENT_OPEN), 0);

    let tcp_lid = LinkId(LINK_TCP_BASE);
    client_assoc.upsert_link(tcp_lid, tcp_link_profile());
    if let Some(l) = client_assoc.link_mut(LINK_SERIAL) {
        l.enabled = false;
    }
    let s1 = client_mux.open_stream(OWNER_ASSOC, true).unwrap();
    client_mux
        .write_stream(
            OWNER_ASSOC,
            s1,
            TxStreamData::Bytes(b"tcp-quic2-req"),
            true,
            &mut client_assoc,
        )
        .unwrap();
    while let Some(out) = client_assoc.pop_outbound_packet() {
        fake.sockets
            .get_mut(&sock_id)
            .unwrap()
            .rx_bytes
            .extend(out.packet.bytes());
    }
    assert_eq!(
        call_socket(&mut state, &host, sock_id, SOCKET_EVENT_READABLE),
        0
    );
    assert_eq!(fake.consumers[&2].received, b"tcp-quic2-req");
    assert!(fake.consumers[&2].fin);

    fake.queue_consumer_reply(2, b"tcp-quic2-resp", true);
    dispatch(&host, &mut state, &tick_call);

    while let Some(wire) = fake.complete_tcp_tx(sock_id) {
        assert_eq!(
            call_socket(&mut state, &host, sock_id, SOCKET_EVENT_WRITE_DONE),
            0
        );
        let mut p = (&pool).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = client_assoc.receive_packet(tcp_lid, p, 3_000).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            client_mux
                .receive_packet(
                    RxMeta {
                        association: OWNER_ASSOC,
                        received_at_us: 3_000,
                    },
                    mux_pkt,
                    &mut client_assoc,
                    &mut client_rx,
                )
                .unwrap();
        }
    }
    for (_link, _peer, wire) in core::mem::take(&mut fake.link_tx) {
        let mut p = (&pool).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = client_assoc.receive_packet(LINK_SERIAL, p, 3_000).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            client_mux
                .receive_packet(
                    RxMeta {
                        association: OWNER_ASSOC,
                        received_at_us: 3_000,
                    },
                    mux_pkt,
                    &mut client_assoc,
                    &mut client_rx,
                )
                .unwrap();
        }
    }
    assert_eq!(client_rx.streams[&s1].0, b"tcp-quic2-resp");
    assert!(client_rx.streams[&s1].1);

    // Send ACK from client so `mod_quic2` releases its retained SACK packets.
    let mut pool_ref = &pool;
    let _ = client_assoc.poll_timers(&mut pool_ref, 50_000);
    while let Some(ack_pkt) = client_assoc.pop_outbound_packet() {
        let _ = call_link(
            &mut state,
            &host,
            &mut fake,
            0,
            ADDR_LINK + 1,
            QUIC2_SERIAL_PORT,
            ack_pkt.packet.bytes(),
        );
    }

    // Check status (method 1).
    let status_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 1,
        flags: 1,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(&host, &mut state, &status_call);
    let reply = fake.replies.pop().unwrap();
    assert_eq!(cbor::get_int(&reply, 7).unwrap(), Some(0)); // 0 errors

    assert_eq!(call_host(&mut state, &host, HOST_STOP), 0);
    assert!(fake.held.is_empty(), "leaked packets: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty());
}
