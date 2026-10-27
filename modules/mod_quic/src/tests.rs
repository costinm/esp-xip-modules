use super::*;
use core::ffi::{c_char, c_void, CStr};
use quic_lite::heap_packet_pool::HeapPacketPool;
use std::{collections::BTreeMap, vec, vec::Vec};
struct Slot {
    bytes: Box<[u8]>,
    refs: usize,
}
struct Physical {
    handle: u32,
    start: usize,
    len: usize,
}
#[derive(Default)]
struct Consumer {
    opening: Vec<u8>,
    bytes: Vec<u8>,
    fin: bool,
}
struct Fake {
    next: u32,
    held: BTreeMap<u32, Slot>,
    physical: Vec<Physical>,
    attached: bool,
    busy: usize,
    attempts: Vec<(usize, Vec<u8>)>,
    now: u64,
    allocations: BTreeMap<usize, core::alloc::Layout>,
    alarms: u32,
    consumers: BTreeMap<u32, Consumer>,
    stream_next: u32,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            next: 0,
            held: BTreeMap::new(),
            physical: Vec::new(),
            attached: false,
            busy: 0,
            attempts: Vec::new(),
            now: 1,
            allocations: BTreeMap::new(),
            alarms: 0,
            consumers: BTreeMap::new(),
            stream_next: 0,
        }
    }
}
unsafe fn f<'a>(p: *mut c_void) -> &'a mut Fake {
    &mut *p.cast()
}
impl Fake {
    fn alloc(&mut self, _owned: bool) -> u32 {
        self.next += 1;
        self.held.insert(
            self.next,
            Slot {
                bytes: vec![0; 1704].into_boxed_slice(),
                refs: 1,
            },
        );
        self.next
    }
    fn complete_physical(&mut self) -> Vec<Vec<u8>> {
        let mut wire = Vec::new();
        for p in core::mem::take(&mut self.physical) {
            wire.push(self.held[&p.handle].bytes[p.start..p.start+p.len].to_vec());
            unsafe { free((self as *mut Fake).cast(),p.handle); }
        }
        wire
    }
}
unsafe extern "C" fn alloc(p: *mut c_void, out: *mut u32) -> i32 {
    *out = f(p).alloc(true);
    0
}
unsafe extern "C" fn span(p: *mut c_void, h: u32, base: *mut *mut u8, cap: *mut usize) -> i32 {
    let s = f(p).held.get_mut(&h).unwrap();
    *base = s.bytes.as_mut_ptr();
    *cap = s.bytes.len();
    0
}
unsafe extern "C" fn keep(p: *mut c_void, h: u32) -> i32 {
    f(p).held.get_mut(&h).unwrap().refs+=1;
    0
}
unsafe extern "C" fn free(p: *mut c_void, h: u32) -> i32 {
    let f=f(p);let slot=f.held.get_mut(&h).expect("double native release");
    slot.refs-=1;if slot.refs==0 {f.held.remove(&h);}
    0
}
unsafe extern "C" fn now(p: *mut c_void) -> u64 {
    f(p).now
}
unsafe extern "C" fn mem_alloc(p: *mut c_void, n: usize) -> *mut c_void {
    let layout = core::alloc::Layout::from_size_align(n, 16).unwrap();
    let ptr = std::alloc::alloc(layout);
    f(p).allocations.insert(ptr as usize, layout);
    ptr.cast()
}
unsafe extern "C" fn mem_free(p: *mut c_void, ptr: *mut c_void) -> i32 {
    let layout = f(p).allocations.remove(&(ptr as usize)).unwrap();
    std::alloc::dealloc(ptr.cast(), layout);
    0
}
unsafe extern "C" fn alarm(p: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: i32, _: i32) -> i32 {
    f(p).alarms += 1;
    f(p).alarms as i32
}
unsafe extern "C" fn cancel(_: *mut c_void, _: i32) -> i32 {
    0
}
unsafe extern "C" fn attach(p: *mut c_void, _: i32) -> i32 {
    let f = f(p);
    f.attached = true;
    0
}
unsafe extern "C" fn detach(p: *mut c_void) -> i32 {
    let f = f(p);
    f.attached = false;
    0
}
unsafe extern "C" fn pool_info(p: *mut c_void, out: *mut PacketPoolInfo) -> i32 {
    let f = f(p);
    *out = PacketPoolInfo {
        size: core::mem::size_of::<PacketPoolInfo>() as u32,
        slot_bytes: 1704,
        headroom: 144,
        max_packet: 1448,
        capacity: 32,
        available: 32u32.saturating_sub(f.held.len() as u32),
    };
    0
}
unsafe extern "C" fn acquire(p: *mut c_void, reserve: u32, out: *mut u32) -> i32 {
    let mut info = PacketPoolInfo::default();
    pool_info(p, &mut info);
    if info.available <= reserve {
        return ERR_BUSY;
    }
    *out = f(p).alloc(true);
    0
}
unsafe extern "C" fn transmit(
    p: *mut c_void,
    h: u32,
    start: u32,
    len: u32,
    _: u32,
    _: u64,
) -> i32 {
    let f = f(p);
    let s = f.held.get(&h).unwrap();
    f.attempts.push((
        s.bytes.as_ptr() as usize,
        s.bytes[start as usize..(start + len) as usize].to_vec(),
    ));
    if f.busy != 0 {
        f.busy -= 1;
        return ERR_BUSY;
    }
    f.physical.push(Physical {
        handle: h,
        start: start as usize,
        len: len as usize,
    });
    0
}
static QUIC: Quic = Quic {
    version: QUIC_VERSION,
    size: core::mem::size_of::<Quic>() as u32,
    attach: Some(attach),
    detach: Some(detach),
    relay_set: None, relay_get: None, relay_remove: None,
    relay_create: None, relay_stats: None, relay_expire: None, relay_idle: None,

};
unsafe extern "C" fn link_info(_: *mut c_void, _: u32, out: *mut LinkInfo) -> i32 {
    let mut info: LinkInfo = core::mem::zeroed();
    info.size = core::mem::size_of::<LinkInfo>() as u32;
    info.provider = LINK_NATIVE;
    info.mtu = 1500;
    info.state = LINK_UP;
    info.name[..3].copy_from_slice(b"udp");
    *out = info;
    0
}
unsafe extern "C" fn link_open(_: *mut c_void, _: *const c_char, _: i32, method: i32) -> i32 {
    assert_eq!(method, LINK_EGRESS);
    (ADDR_LINK + 1) as i32
}
unsafe extern "C" fn link_close(_: *mut c_void, _: u32) -> i32 { 0 }
static LINK: Link = Link {
    open: Some(link_open), close: Some(link_close),
    version: LINK_VERSION,
    size: core::mem::size_of::<Link>() as u32,
    info: Some(link_info),
    ..unsafe { core::mem::zeroed() }
};
unsafe extern "C" fn stream_open(p: *mut c_void, bytes: *const u8, len: usize) -> i32 {
    assert!(bytes.is_null(), "transport must leave envelope framing to runtime");
    assert_eq!(len,0);
    let f = f(p);
    f.stream_next += 1;
    let id = f.stream_next;
    f.consumers.insert(id, Consumer::default());
    id as i32
}
unsafe extern "C" fn stream_feed(
    p: *mut c_void,
    id: u32,
    bytes: *const u8,
    len: usize,
    fin: u32,
) -> i32 {
    let f = f(p);
    // Hold consumption on alternate turns to exercise exact-chunk retry.
    if len != 0 && (f.now / 10000) % 2 == 0 {
        return 0;
    }
    let c = f.consumers.get_mut(&id).unwrap();
    if len != 0 {
        // This fake accepts the fixture's known 10-byte opening. Actual runtime
        // framing/dispatch is covered by the native C broker regression.
        let bytes=core::slice::from_raw_parts(bytes,len);
        let opening=(10-c.opening.len()).min(bytes.len());
        c.opening.extend_from_slice(&bytes[..opening]);
        if c.opening.len()==10 {
            assert_eq!(c.opening,[0xa4,1,0x18,44,2,1,3,1,5,0xa0]);
        }
        c.bytes.extend_from_slice(&bytes[opening..]);
    }
    if fin != 0 {
        c.fin = true;
    }
    len as i32
}
unsafe extern "C" fn stream_take(p: *mut c_void, id: u32, out: *mut StreamPacket) -> i32 {
    let f = f(p);
    let Some(c) = f.consumers.get_mut(&id) else {
        return ERR_NOTFOUND;
    };
    if c.bytes.len() < 500 && !c.fin {
        return 0;
    }
    let n = c.bytes.len().min(1500);
    let bytes: Vec<_> = c.bytes.drain(..n).collect();
    let fin = c.fin && c.bytes.is_empty();
    let h = f.alloc(true);
    f.held.get_mut(&h).unwrap().bytes[144..144 + n].copy_from_slice(&bytes);
    *out = StreamPacket {
        packet: h,
        len: n as u32,
        fin: fin as u32,
    };
    1
}
unsafe extern "C" fn stream_close(p: *mut c_void, id: u32) -> i32 {
    f(p).consumers.remove(&id);
    0
}
unsafe extern "C" fn stream_written(_: *mut c_void, _: u32) -> i32 {
    0
}
static STREAM: Stream = Stream {
    listen:None,
    version: 1,
    size: core::mem::size_of::<Stream>() as u32,
    open: Some(stream_open),
    feed: Some(stream_feed),
    close: Some(stream_close),
    take: Some(stream_take),
    write_pkt: None,
    written: Some(stream_written),
};
unsafe extern "C" fn component(_: *mut c_void, name: *const c_char) -> *const c_void {
    match CStr::from_ptr(name).to_bytes() {
        b"quic" => (&QUIC as *const Quic).cast(),
        b"link" => (&LINK as *const Link).cast(),
        b"stream" => (&STREAM as *const Stream).cast(),
        _ => core::ptr::null(),
    }
}
fn host(fake: &mut Fake) -> Host {
    let mut h: Host = unsafe { core::mem::zeroed() };
    h.abi_version = ABI_VERSION;
    h.size = core::mem::size_of::<Host>() as u32;
    h.user = (fake as *mut Fake).cast();
    h.pkt_alloc = Some(alloc);
    h.pkt_span = Some(span);
    h.pkt_buf=Some(buf);
    h.pkt_pool_info=Some(pool_info);
    h.pkt_acquire=Some(acquire);
    h.pkt_tx=Some(transmit);
    h.pkt_keep = Some(keep);
    h.pkt_free = Some(free);
    h.now_us = Some(now);
    h.mem_alloc = Some(mem_alloc);
    h.mem_free = Some(mem_free);
    h.alarm_set = Some(alarm);
    h.alarm_cancel = Some(cancel);
    h.get_component = Some(component);
    h
}
fn receive(state: &mut State, hw: &Hw, fake: &mut Fake, bytes: &[u8]) {
    let h = fake.alloc(false);
    let slot = fake.held.get_mut(&h).unwrap();
    slot.bytes[144..144 + bytes.len()].copy_from_slice(bytes);
    let ptr = unsafe { slot.bytes.as_ptr().add(144) };
    let call = CallView {
        component: COMPONENT,
        method: RX,
        value: -43,
        flags: CALL_LINK,
        data: unsafe { core::slice::from_raw_parts(ptr, bytes.len()) },
        time_us: fake.now as u32,
        pkt: h,
        from: ADDR_LINK + 1,
        id: 1,
        request_id: None,
        src: None,
    };
    state.handle_owned(hw,&call,Some(unsafe {Packet::from_raw(*hw,h)}));
}
fn tick(state: &mut State, hw: &Hw) {
    let call = CallView {
        component: COMPONENT,
        method: TICK,
        value: 0,
        flags: 0,
        data: &[],
        time_us: 0,
        pkt: 0,
        from: 0,
        id: 0,
        request_id: None,
        src: None,
    };
    state.handle(hw, &call);
    let ready=CallView {flags:CALL_LINK|CALL_LINK_EVENT|(LINK_EVENT_READY<<8),from:ADDR_LINK+1,..call};
    state.handle(hw,&ready);
}
type ClientPool = HeapPacketPool<16, 1704>;
struct ClientBearer {
    context: Arc<Mutex<Option<BearerContext<ClientPool>>>>,
    wire: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl PacketEgress<<ClientPool as PacketPool>::Buffer> for ClientBearer {
    fn submit(
        &mut self,
        _: PeerL2Address,
        s: EgressSubmission<<ClientPool as PacketPool>::Buffer>,
    ) -> Result<(), PacketSubmitError<<ClientPool as PacketPool>::Buffer>> {
        self.wire.lock().push(s.packet().bytes().to_vec());
        s.complete(PacketSendOutcome::Sent, 1);
        Ok(())
    }
}
impl PacketBearer<ClientPool> for ClientBearer {
    type AttachError = ();
    fn info(&self) -> BearerInfo {
        BearerInfo {
            name: BearerName::new("test").unwrap(),
            max_packet_size: 1100,
            prefix_required: 80,
            suffix_required: 24,
            requires_packet_encryption: false,
            secure_link: false,
            nominal_bitrate_bps: 1000000,
            local_mac: None,
        }
    }
    fn attach(&mut self, c: BearerContext<ClientPool>) -> Result<(), ()> {
        *self.context.lock() = Some(c);
        Ok(())
    }
}
#[test]
fn opaque_stream_delivery_preserves_bytes_under_loss_backpressure_and_busy() {
    echo_fixture(false,4096);
    echo_fixture(true,4096);
}
#[test]
fn large_owned_streams_release_all_holders() { echo_fixture(false,65536); }
fn echo_fixture(loss: bool, size:usize) {
    static SERIAL:std::sync::Mutex<()>=std::sync::Mutex::new(());
    let _serial=SERIAL.lock().unwrap();
    static CLIENT: ClientPool = ClientPool::new();
    CLIENT.resize(16).unwrap();
    let mut fake = Fake::default();
    let h = host(&mut fake);
    let hw = unsafe { Hw::new(&h) }.unwrap();
    let mut state = State::default();
    state.start(&hw).unwrap();
    assert!(
        unsafe { &*(state.context as *const Context) }
            .core
            .is_none(),
        "idle endpoint must not allocate QUIC"
    );
    let cx = unsafe { &mut *(state.context as *mut Context) };
    assert!(cx.stats.is_none(), "diagnostic recording must be opt-in");
    if loss { cx.configure_stats(&[0xa1,1,0xf5]).unwrap(); }
    let context = Arc::new(Mutex::new(None));
    let wire = Arc::new(Mutex::new(Vec::new()));
    let mut client: NoStdRuntime<_, 1100> = NoStdRuntime::new(
        QuicNode::new(&CLIENT),
        ConnectionLimits::with_receive_profile(4096, 2048, 2),
    );
    let bid = client
        .add_bearer(ClientBearer {
            context: context.clone(),
            wire: wire.clone(),
        })
        .unwrap();
    let association = client
        .associate(
            PacketMeta {
                bearer: bid,
                peer_l2_address: PeerL2Address::new(1).unwrap(),
                received_at_us: 1,
            },
            1,
        )
        .unwrap();
    let mut streams = None;
    let input = [vec![0x7e; size], vec![0x7d; size],vec![0x55;1537]];
    let header = [0xa4, 1, 0x18, 44, 2, 1, 3, 1, 5, 0xa0];
    let wire_input = input.each_ref().map(|body| {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(body);
        bytes
    });
    let mut sent = [0usize; 3];
    let mut received = [Vec::new(), Vec::new(),Vec::new()];
    let mut ids = [0; 3];
    fake.busy = 3;
    let mut dropped = false;
    for step in 0..3000 {
        fake.now = step * 10000 + 1;
        for packet in core::mem::take(&mut *wire.lock()) {
            receive(&mut state, &hw, &mut fake, &packet);
        }
        tick(&mut state, &hw);
        let c = context.lock().clone().unwrap();
        let mut returning = fake.complete_physical();
        if loss {
            returning.reverse();
        }
        for packet in returning {
            if loss && !dropped && streams.is_some() && packet.len() > 100 {
                dropped = true;
                continue;
            }
            let owned = PacketPool::build_packet(&CLIENT, 144, 0, |out| {
                out[..packet.len()].copy_from_slice(&packet);
                Ok::<_, ()>(packet.len())
            })
            .unwrap();
            c.enqueue_packet(
                PacketMeta {
                    bearer: bid,
                    peer_l2_address: PeerL2Address::new(1).unwrap(),
                    received_at_us: fake.now,
                },
                owned,
            );
        }
        client.advance_time(fake.now).ok();
        client.progress_all().ok();
        if streams.is_none() && client.association_is_established(association) {
            let a = client.open_stream(association).unwrap();
            let b = client.open_stream(association).unwrap();
            ids = [a.id(), b.id(),0];
            streams = Some(vec![a,b]);
        }
        if let Some(streams) = streams.as_mut() {
            if streams.len()==2 && received[0].len()==input[0].len() && received[1].len()==input[1].len(){
                if let Ok(c)=client.open_stream(association){ids[2]=c.id();streams.push(c);}
            }
            for i in 0..streams.len() {
                if sent[i] < wire_input[i].len() {
                    if let Ok(n) =
                        client.write_stream_and_finish(&mut streams[i], &wire_input[i][sent[i]..])
                    {
                        sent[i] += n;
                    }
                }
            }
        }
        while let Some(chunk) = client.next_stream_chunk() {
            let i = ids.iter().position(|id| *id == chunk.stream.id()).unwrap();
            assert_eq!(chunk.offset as usize, received[i].len());
            received[i].extend(chunk.bytes);
        }
        if received.iter().zip(input.iter()).all(|(a,b)|a.len()==b.len()) {
            break;
        }
    }
    assert!(
        !loss || dropped,
        "loss fixture did not exercise a dropped packet"
    );
    assert_eq!(
        received.iter().map(Vec::len).collect::<Vec<_>>(),
        input.iter().map(Vec::len).collect::<Vec<_>>(),
        "stream dispatch did not complete; errors {} last {} sent {:?}",
        unsafe { &*(state.context as *const Context) }.errors,
        unsafe { &*(state.context as *const Context) }.last_error,
        sent
    );
    assert_eq!(received, input);
    assert!(fake.attempts.len() > 3);
    assert_eq!(
        fake.attempts[0], fake.attempts[1],
        "BUSY must retain exact backing pointer and prepared bytes"
    );
    assert_eq!(fake.attempts[1], fake.attempts[2]);
    // FIN finishes a stream, not the association. The portable node expires
    // a silent peer after its real 30-second idle timeout. No device wall wait.
    fake.now += 31_000_000;
    fake.complete_physical();
    for _ in 0..8 {
        tick(&mut state, &hw);
        fake.complete_physical();
    }
    let cx = unsafe { &mut *(state.context as *mut Context) };
    if loss {
        assert!(cx.core.is_none());
        let saved = cx.stats.as_deref().unwrap();
        assert!(saved.received_packets > 0 && saved.sent_packets > 0);
        let mut bytes = [0;256]; let mut w = Writer::new(&mut bytes);
        cx.write_stats(&mut w); let bytes=w.finish().unwrap();
        assert_eq!(cbor::get_int(bytes,3).unwrap(),Some(saved.received_packets as i64));
        assert!(matches!(cbor::get(bytes,2).unwrap().unwrap().next(),Some(cbor::Item::Bool(false))));
        cx.configure_stats(&[0xa1,1,0xf4]).unwrap();
    }
    assert!(cx.stats.is_none(), "disable must release the snapshot");
    let context_state = unsafe { &*(state.context as *const Context) };
    assert!(
        context_state.core.is_none(),
        "idle association must release the entire QUIC core and scoped pool"
    );
    assert_eq!(context_state.alarm, -1, "idle endpoint must stop its timer");
    assert!(fake.held.is_empty());
    assert!(fake.physical.is_empty());
    assert_eq!(
        fake.allocations.len(),
        1,
        "only small endpoint Context remains until stop"
    );
    state.stop(&hw);
    fake.complete_physical();
    assert!(fake.held.is_empty());
    assert!(fake.allocations.is_empty());
    drop(client);
    drop(context);
    drop(wire);
    CLIENT.resize(0).unwrap();
}
#[test]
fn stopping_disowns_physical_frames_and_restart_never_rebinds_old_epoch() {
    let mut fake = Fake::default();
    let h = host(&mut fake);
    let hw = unsafe { Hw::new(&h) }.unwrap();
    let mut state = State::default();
    for _ in 0..3 {
        state.start(&hw).unwrap();
        state.stop(&hw);
        assert!(fake.allocations.is_empty());
        assert!(fake.held.is_empty());
    }
    state.start(&hw).unwrap();
    let ptr = state.context;
    let native = Native::get(hw).unwrap();
    let pool = Box::new(Pool { native });
    let p: &'static Pool = unsafe { &*(pool.as_ref() as *const Pool) };
    let packet = PacketPool::build_packet(p, 144, 0, |out| {
        out[..5].copy_from_slice(b"hello");
        Ok::<_, ()>(5)
    })
    .unwrap();
    let (buffer, range) = packet.into_parts();
    assert_eq!(hw.pkt_keep(buffer.handle()),0);
    assert_eq!(unsafe {transmit(hw.ctx(),buffer.handle(),range.start as u32,5,ADDR_LINK+1,1)},0);
    drop(buffer); // only the driver reference survives protocol close
    state.stop(&hw);
    assert_eq!(state.context, 0);
    assert!(!fake.physical.is_empty());
    state.start(&hw).unwrap();
    assert_ne!(state.context, 0);
    let _ = ptr;
    fake.complete_physical();
    drop(pool);
    state.stop(&hw);
    assert!(fake.allocations.is_empty());
    assert!(fake.held.is_empty());
}

unsafe extern "C" fn buf(p:*mut c_void,h:u32,ptr:*mut *mut u8,max:*mut usize)->i32 {
    let s=f(p).held.get_mut(&h).unwrap();if s.refs!=1 {return ERR_BUSY;}
    *ptr=s.bytes.as_mut_ptr().add(144);*max=1500;0
}
