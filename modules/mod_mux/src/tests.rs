use super::*;
use core::ffi::{c_char, c_int, c_void, CStr};
use quic_base::{StaticPacketLease, StaticPacketPool};
use quic_mux::{
    AssociationEgress, AssociationId, MuxCallbacks, MuxLimits, PacketPool, PoolPacket, QuicMux,
    RxMeta, RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId,
    TxPacketMeta, TxStreamData,
};
use std::collections::{BTreeMap, VecDeque};
use std::{boxed::Box, vec, vec::Vec};
use xip_abi::{
    cbor, Call, Host, PacketPoolInfo, Stream, StreamPacket, ABI_VERSION, ADDR_LINK, CALL_LINK,
    ERR_AGAIN, ERR_ARG, ERR_BUSY, HOST_COMPONENT, HOST_START, HOST_STOP, PKT_HEADROOM, PKT_MAX,
    PKT_SLOT_MIN, SOCKET_EVENT_CLOSED, SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE,
    SOCKET_EVENT_WRITE_DONE, STREAM_VERSION,
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

struct FakeSocket {
    rx_bytes: VecDeque<u8>,
    tx_inflight: Option<(u32, usize, usize)>,
    tx_completed: Vec<Vec<u8>>,
    closed: bool,
}

struct FakeHost {
    next_pkt: u32,
    held: BTreeMap<u32, Slot>,
    serial_tx: Vec<(u32, u64, Vec<u8>)>,
    serial_busy: bool,
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

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            next_pkt: 0,
            held: BTreeMap::new(),
            serial_tx: Vec::new(),
            serial_busy: false,
            sockets: BTreeMap::new(),
            consumers: BTreeMap::new(),
            next_stream: 0,
            feed_limit_default: None,
            allocations: BTreeMap::new(),
            alarms: BTreeMap::new(),
            next_alarm: 1,
            replies: Vec::new(),
            now_us: 1_000,
        }
    }
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
unsafe extern "C" fn h_pkt_acquire(u: *mut c_void, reserve: u32, out: *mut u32) -> c_int {
    let f = fh(u);
    let avail = 64usize.saturating_sub(f.held.len());
    if avail <= reserve as usize {
        return xip_abi::ERR_NOMEM;
    }
    *out = f.alloc_pkt();
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
    if f.serial_busy {
        return ERR_BUSY;
    }
    let data = f.held[&pkt].bytes[start as usize..(start + len) as usize].to_vec();
    f.serial_tx.push((link, peer, data));
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
        if f.serial_busy {
            return ERR_BUSY;
        }
        let data = s.bytes[s.start..s.start + len].to_vec();
        f.serial_tx.push((dst, id, data));
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
    h.pkt_acquire = Some(h_pkt_acquire);
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

fn host_call(method: u32) -> Call {
    let mut c: Call = unsafe { core::mem::zeroed() };
    c.size = core::mem::size_of::<Call>() as u32;
    c.component = HOST_COMPONENT as i32;
    c.method = method as i32;
    c
}

struct VecEgress {
    mtu: usize,
    frames: Vec<Vec<u8>>,
}

impl<P: PoolPacket> AssociationEgress<P> for VecEgress {
    fn max_writable_frame_len(&self, _assoc: AssociationId) -> usize {
        self.mtu
    }
    fn send_packet(
        &mut self,
        _meta: TxPacketMeta,
        packet: P,
    ) -> Result<(), quic_mux::LinkSendError<P>> {
        self.frames.push(packet.bytes().to_vec());
        Ok(())
    }
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
    fn on_association_broken(&mut self, _: AssociationId, _: quic_mux::AssociationBrokenReason) {}
}

#[test]
fn ppp_serial_stream_roundtrip_and_clean_stop() {
    let mut fake = FakeHost::default();
    let mut state = State::default();

    {
        let host = make_host(&mut fake);
        assert_eq!(dispatch(&host, &mut state, &host_call(HOST_START)), 0);
    }

    // Peer client `QuicMux` encodes a stream frame for `OWNER_ASSOC` (`AssociationId(1)`).
    let pool: StaticPacketPool<16> = StaticPacketPool::new();
    let mut client = QuicMux::new(&pool, MuxLimits::default());
    let assoc = links::OWNER_ASSOC;
    client.add_association(assoc, true).unwrap();
    let sid = client.open_stream(assoc, true).unwrap();
    let mut client_egress = VecEgress {
        mtu: links::SERIAL_FRAME_MTU,
        frames: Vec::new(),
    };
    let req = b"hello-over-ppp-245";
    client
        .write_stream(assoc, sid, TxStreamData::Bytes(req), true, &mut client_egress)
        .unwrap();
    let mux_frame = client_egress.frames.pop().unwrap();

    // Deliver PPP serial packet `[mux_frame...]` directly on `--port 245` (no assoc_id prefix).
    let pkt = fake.alloc_pkt();
    let start = PKT_HEADROOM as usize;
    let ptr = {
        let slot = fake.held.get_mut(&pkt).unwrap();
        slot.bytes[start..start + mux_frame.len()].copy_from_slice(&mux_frame);
        unsafe { slot.bytes.as_ptr().add(start) }
    };
    {
        let host = make_host(&mut fake);
        let mut call: Call = unsafe { core::mem::zeroed() };
        call.size = core::mem::size_of::<Call>() as u32;
        call.component = COMPONENT;
        call.method = 0;
        call.flags = CALL_LINK;
        call.from = ADDR_LINK;
        call.id = links::MUX_SERIAL_PORT;
        call.pkt = pkt;
        call.data = ptr;
        call.len = mux_frame.len();
        assert_eq!(dispatch(&host, &mut state, &call), 0);
    }

    assert_eq!(fake.consumers[&1].received, req);
    assert!(fake.consumers[&1].fin);

    // Queue a full 1,500-byte response packet from the consumer (`PKT_MAX`), verifying
    // `mod_mux` splits the small prefix + sends the bulk zero-copy within `SERIAL_FRAME_MTU` (1,500)!
    let resp: Vec<u8> = (0..1500).map(|i| (i % 251) as u8).collect();
    fake.queue_consumer_reply(1, &resp, true);
    {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }

    assert_eq!(fake.consumers[&1].written_calls, 1);
    assert!(fake.serial_tx.len() >= 2);

    let mut client_rx = ClientRx::default();
    for (link, port, raw) in fake.serial_tx.drain(..) {
        assert_eq!(link, ADDR_LINK);
        assert_eq!(port, links::MUX_SERIAL_PORT);
        assert!(raw.len() <= PKT_MAX as usize);
        let mut rx_pkt = (&pool).alloc().unwrap();
        rx_pkt.append(&raw).unwrap();
        client
            .receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: 2_000,
                },
                rx_pkt,
                &mut client_egress,
                &mut client_rx,
            )
            .unwrap();
    }
    let (got_bytes, got_fin) = &client_rx.streams[&sid];
    assert_eq!(got_bytes, &resp);
    assert!(*got_fin);

    // Verify `mux.status` (method 1).
    {
        let host = make_host(&mut fake);
        let mut req_call: Call = unsafe { core::mem::zeroed() };
        req_call.size = core::mem::size_of::<Call>() as u32;
        req_call.component = COMPONENT;
        req_call.method = 1;
        req_call.flags = 1; // wants_response
        assert_eq!(dispatch(&host, &mut state, &req_call), 0);
    }
    let status_cbor = fake.replies.pop().unwrap();
    assert_eq!(cbor::get_int(&status_cbor, 1), Ok(Some(1))); // 1 association (`OWNER_ASSOC`)
    assert_eq!(cbor::get_int(&status_cbor, 7), Ok(Some(0))); // 0 errors

    // Stop module and assert zero memory/packet/alarm leaks!
    {
        let host = make_host(&mut fake);
        assert_eq!(dispatch(&host, &mut state, &host_call(HOST_STOP)), 0);
    }
    assert!(fake.held.is_empty(), "packet leak: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty(), "heap leak");
    assert!(fake.alarms.is_empty(), "alarm leak");
}

#[test]
fn tcp_fragmented_ingress_tx_coalescing_and_multipath_failover() {
    let mut fake = FakeHost::default();
    fake.feed_limit_default = Some(7); // Exercise partial consumer consumption + `resume_stream`!
    let mut state = State::default();

    {
        let host = make_host(&mut fake);
        assert_eq!(dispatch(&host, &mut state, &host_call(HOST_START)), 0);
    }

    let pool: StaticPacketPool<16> = StaticPacketPool::new();
    let mut client = QuicMux::new(&pool, MuxLimits::default());
    let assoc = links::OWNER_ASSOC;
    client.add_association(assoc, true).unwrap();
    let s1 = client.open_stream(assoc, true).unwrap();
    let s2 = client.open_stream(assoc, true).unwrap();
    let mut client_egress = VecEgress {
        mtu: links::TCP_FRAME_MTU,
        frames: Vec::new(),
    };
    client
        .write_stream(assoc, s1, TxStreamData::Bytes(b"fragmented-tcp-1"), true, &mut client_egress)
        .unwrap();
    client
        .write_stream(assoc, s2, TxStreamData::Bytes(b"fragmented-tcp-2"), true, &mut client_egress)
        .unwrap();

    // Raw self-delimiting QUIC frames directly on TCP (no outer wrapper).
    let mut tcp_wire = Vec::new();
    for f in client_egress.frames.drain(..) {
        tcp_wire.extend_from_slice(&f);
    }

    let sock_id = 42u32;
    fake.sockets.insert(
        sock_id,
        FakeSocket {
            rx_bytes: VecDeque::new(),
            tx_inflight: None,
            tx_completed: Vec::new(),
            closed: false,
        },
    );

    // Open socket and drip-feed bytes in 5-byte fragments across READABLE events.
    {
        let host = make_host(&mut fake);
        let mut open: Call = unsafe { core::mem::zeroed() };
        open.size = core::mem::size_of::<Call>() as u32;
        open.component = COMPONENT;
        open.id = u64::from(sock_id);
        open.value = SOCKET_EVENT_OPEN as i32;
        assert_eq!(dispatch(&host, &mut state, &open), 0);
    }
    for chunk in tcp_wire.chunks(5) {
        fake.sockets
            .get_mut(&sock_id)
            .unwrap()
            .rx_bytes
            .extend(chunk.iter().copied());
        let host = make_host(&mut fake);
        let mut readable: Call = unsafe { core::mem::zeroed() };
        readable.size = core::mem::size_of::<Call>() as u32;
        readable.component = COMPONENT;
        readable.id = u64::from(sock_id);
        readable.value = SOCKET_EVENT_READABLE as i32;
        assert_eq!(dispatch(&host, &mut state, &readable), 0);
    }
    // Pump ticks so `resume_stream` drains the 7-byte consumer feed limit completely.
    for _ in 0..6 {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }
    assert_eq!(fake.consumers[&1].received, b"fragmented-tcp-1");
    assert!(fake.consumers[&1].fin);
    assert_eq!(fake.consumers[&2].received, b"fragmented-tcp-2");
    assert!(fake.consumers[&2].fin);

    // Also register PPP serial for the SAME `OWNER_ASSOC` (`AssociationId(1)`) using a raw Ping frame (`0x01`)!
    let ping_pkt = fake.alloc_pkt();
    let ping_wire = [0x01u8];
    let start = PKT_HEADROOM as usize;
    let ptr = {
        let slot = fake.held.get_mut(&ping_pkt).unwrap();
        slot.bytes[start..start + 1].copy_from_slice(&ping_wire);
        unsafe { slot.bytes.as_ptr().add(start) }
    };
    {
        let host = make_host(&mut fake);
        let mut call: Call = unsafe { core::mem::zeroed() };
        call.size = core::mem::size_of::<Call>() as u32;
        call.component = COMPONENT;
        call.flags = CALL_LINK;
        call.from = ADDR_LINK;
        call.id = links::MUX_SERIAL_PORT;
        call.pkt = ping_pkt;
        call.data = ptr;
        call.len = 1;
        assert_eq!(dispatch(&host, &mut state, &call), 0);
    }

    // Queue 3 small replies at once while TCP `tx_inflight` holds packet #1:
    // - Stream 1 sends packet #1 -> goes into `tx_inflight` on TCP.
    // - Stream 2 sends packet #2 (small) -> goes into `pending_tx` on TCP.
    // - Stream 3 sends packet #3 (600B > 1/3 MTU) -> spills over onto PPP serial!
    let s3 = client.open_stream(assoc, true).unwrap();
    client
        .write_stream(assoc, s3, TxStreamData::Bytes(b"s3"), true, &mut client_egress)
        .unwrap();
    let s3_frame = client_egress.frames.pop().unwrap();
    {
        fake.sockets
            .get_mut(&sock_id)
            .unwrap()
            .rx_bytes
            .extend(s3_frame);
        let host = make_host(&mut fake);
        let mut readable: Call = unsafe { core::mem::zeroed() };
        readable.size = core::mem::size_of::<Call>() as u32;
        readable.component = COMPONENT;
        readable.id = u64::from(sock_id);
        readable.value = SOCKET_EVENT_READABLE as i32;
        assert_eq!(dispatch(&host, &mut state, &readable), 0);
    }

    fake.queue_consumer_reply(1, &[b'A'; 600], true);
    fake.queue_consumer_reply(2, &[b'B'; 600], true);
    fake.queue_consumer_reply(3, &[b'C'; 600], true);
    {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }

    // Complete first TCP write, then second TCP write.
    let mut tcp_out = Vec::new();
    while let Some(raw) = fake.complete_tcp_tx(sock_id) {
        tcp_out.push(raw);
        let host = make_host(&mut fake);
        let mut wd: Call = unsafe { core::mem::zeroed() };
        wd.size = core::mem::size_of::<Call>() as u32;
        wd.component = COMPONENT;
        wd.id = u64::from(sock_id);
        wd.value = SOCKET_EVENT_WRITE_DONE as i32;
        assert_eq!(dispatch(&host, &mut state, &wd), 0);
    }
    assert!(
        !fake.serial_tx.is_empty(),
        "packet #3 should spill over onto multipath PPP serial while TCP tx_inflight + pending_tx are occupied"
    );

    let mut client_rx = ClientRx::default();
    for raw in tcp_out {
        let mut rx_pkt = (&pool).alloc().unwrap();
        rx_pkt.append(&raw).unwrap();
        client
            .receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: 5_000,
                },
                rx_pkt,
                &mut client_egress,
                &mut client_rx,
            )
            .unwrap();
    }
    for (_, _, raw) in fake.serial_tx.drain(..) {
        let mut rx_pkt = (&pool).alloc().unwrap();
        rx_pkt.append(&raw).unwrap();
        client
            .receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: 5_100,
                },
                rx_pkt,
                &mut client_egress,
                &mut client_rx,
            )
            .unwrap();
    }
    assert_eq!(client_rx.streams[&s1].0, &[b'A'; 600]);
    assert_eq!(client_rx.streams[&s2].0, &[b'B'; 600]);
    assert_eq!(client_rx.streams[&s3].0, &[b'C'; 600]);

    // Also verify TX small-packet coalescing on `sock_id` when 3 small (< 1/3 MTU) replies are queued!
    let c1 = client.open_stream(assoc, true).unwrap();
    let c2 = client.open_stream(assoc, true).unwrap();
    let c3 = client.open_stream(assoc, true).unwrap();
    for (sid, msg) in [(c1, &b"c1"[..]), (c2, &b"c2"[..]), (c3, &b"c3"[..])] {
        client
            .write_stream(assoc, sid, TxStreamData::Bytes(msg), true, &mut client_egress)
            .unwrap();
    }
    for f in client_egress.frames.drain(..) {
        fake.sockets.get_mut(&sock_id).unwrap().rx_bytes.extend(f);
    }
    {
        let host = make_host(&mut fake);
        let mut readable2: Call = unsafe { core::mem::zeroed() };
        readable2.size = core::mem::size_of::<Call>() as u32;
        readable2.component = COMPONENT;
        readable2.id = u64::from(sock_id);
        readable2.value = SOCKET_EVENT_READABLE as i32;
        assert_eq!(dispatch(&host, &mut state, &readable2), 0);
    }
    fake.queue_consumer_reply(4, b"coal-1", true);
    fake.queue_consumer_reply(5, b"coal-2", true);
    fake.queue_consumer_reply(6, b"coal-3", true);
    {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }
    let mut tcp2_out = Vec::new();
    while let Some(raw) = fake.complete_tcp_tx(sock_id) {
        tcp2_out.push(raw);
        let host = make_host(&mut fake);
        let mut wd: Call = unsafe { core::mem::zeroed() };
        wd.size = core::mem::size_of::<Call>() as u32;
        wd.component = COMPONENT;
        wd.id = u64::from(sock_id);
        wd.value = SOCKET_EVENT_WRITE_DONE as i32;
        assert_eq!(dispatch(&host, &mut state, &wd), 0);
    }
    assert!(
        tcp2_out.len() <= 2,
        "3 small replies on TCP should coalesce into pending_tx (got {} writes)",
        tcp2_out.len()
    );
    for raw in tcp2_out {
        let mut rx_pkt = (&pool).alloc().unwrap();
        rx_pkt.append(&raw).unwrap();
        client
            .receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: 5_500,
                },
                rx_pkt,
                &mut client_egress,
                &mut client_rx,
            )
            .unwrap();
    }
    assert_eq!(client_rx.streams[&c1].0, b"coal-1");
    assert_eq!(client_rx.streams[&c2].0, b"coal-2");
    assert_eq!(client_rx.streams[&c3].0, b"coal-3");

    // Now close the TCP socket (`SOCKET_EVENT_CLOSED`) and verify `OWNER_ASSOC` stays alive
    // and seamlessly fails over to PPP serial!
    {
        let host = make_host(&mut fake);
        let mut closed: Call = unsafe { core::mem::zeroed() };
        closed.size = core::mem::size_of::<Call>() as u32;
        closed.component = COMPONENT;
        closed.id = u64::from(sock_id);
        closed.value = SOCKET_EVENT_CLOSED as i32;
        assert_eq!(dispatch(&host, &mut state, &closed), 0);
    }

    let s4 = client.open_stream(assoc, true).unwrap();
    client
        .write_stream(assoc, s4, TxStreamData::Bytes(b"after-tcp-drop"), true, &mut client_egress)
        .unwrap();
    let s4_frame = client_egress.frames.pop().unwrap();
    let s4_pkt = fake.alloc_pkt();
    let ptr = {
        let slot = fake.held.get_mut(&s4_pkt).unwrap();
        slot.bytes[start..start + s4_frame.len()].copy_from_slice(&s4_frame);
        unsafe { slot.bytes.as_ptr().add(start) }
    };
    {
        let host = make_host(&mut fake);
        let mut call: Call = unsafe { core::mem::zeroed() };
        call.size = core::mem::size_of::<Call>() as u32;
        call.component = COMPONENT;
        call.flags = CALL_LINK;
        call.from = ADDR_LINK;
        call.id = links::MUX_SERIAL_PORT;
        call.pkt = s4_pkt;
        call.data = ptr;
        call.len = s4_frame.len();
        assert_eq!(dispatch(&host, &mut state, &call), 0);
    }
    for _ in 0..3 {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }
    fake.queue_consumer_reply(7, b"serial-failover-ok", true);
    {
        let host = make_host(&mut fake);
        let mut tick: Call = unsafe { core::mem::zeroed() };
        tick.size = core::mem::size_of::<Call>() as u32;
        tick.component = COMPONENT;
        tick.method = TICK;
        assert_eq!(dispatch(&host, &mut state, &tick), 0);
    }
    assert!(!fake.serial_tx.is_empty());
    for (_, _, raw) in fake.serial_tx.drain(..) {
        let mut rx_pkt = (&pool).alloc().unwrap();
        rx_pkt.append(&raw).unwrap();
        client
            .receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: 6_000,
                },
                rx_pkt,
                &mut client_egress,
                &mut client_rx,
            )
            .unwrap();
    }
    assert_eq!(client_rx.streams[&s4].0, b"serial-failover-ok");

    {
        let host = make_host(&mut fake);
        assert_eq!(dispatch(&host, &mut state, &host_call(HOST_STOP)), 0);
    }
    assert!(fake.held.is_empty());
    assert!(fake.allocations.is_empty());
    assert!(fake.alarms.is_empty());
}
