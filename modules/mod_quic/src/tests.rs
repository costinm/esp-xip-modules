use super::*;
use core::ffi::{c_char, c_void, CStr};
use quic_lite::heap_packet_pool::HeapPacketPool;
use std::{collections::BTreeMap, vec, vec::Vec};
struct Slot {
    bytes: Box<[u8]>,
    owned: bool,
}
struct Physical {
    slot: Slot,
    start: usize,
    len: usize,
    ticket: u32,
    epoch: u32,
}
struct Fake {
    next: u32,
    held: BTreeMap<u32, Slot>,
    physical: Vec<Physical>,
    done: Vec<(QuicTxCompletion, Slot)>,
    epoch: u32,
    attached: bool,
    busy: usize,
    attempts: Vec<(usize, Vec<u8>)>,
    now: u64,
    allocations: BTreeMap<usize, core::alloc::Layout>,
    alarms: u32,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            next: 0,
            held: BTreeMap::new(),
            physical: Vec::new(),
            done: Vec::new(),
            epoch: 0,
            attached: false,
            busy: 0,
            attempts: Vec::new(),
            now: 1,
            allocations: BTreeMap::new(),
            alarms: 0,
        }
    }
}
unsafe fn f<'a>(p: *mut c_void) -> &'a mut Fake {
    &mut *p.cast()
}
impl Fake {
    fn alloc(&mut self, owned: bool) -> u32 {
        self.next += 1;
        self.held.insert(
            self.next,
            Slot {
                bytes: vec![0; 1704].into_boxed_slice(),
                owned,
            },
        );
        self.next
    }
    fn complete_physical(&mut self) -> Vec<Vec<u8>> {
        let mut wire = Vec::new();
        for p in core::mem::take(&mut self.physical) {
            wire.push(p.slot.bytes[p.start..p.start + p.len].to_vec());
            if self.attached && p.epoch == self.epoch {
                self.done.push((
                    QuicTxCompletion {
                        size: core::mem::size_of::<QuicTxCompletion>() as u32,
                        ticket: p.ticket,
                        start: p.start as u32,
                        len: p.len as u32,
                        link: ADDR_LINK + 1,
                        peer: 1,
                        outcome: 0,
                        elapsed_us: 1,
                        ..Default::default()
                    },
                    p.slot,
                ));
            }
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
    let f = f(p);
    if f.held.values().filter(|s| s.owned).count() >= 4 {
        return ERR_BUSY;
    }
    f.held.get_mut(&h).unwrap().owned = true;
    0
}
unsafe extern "C" fn free(p: *mut c_void, h: u32) -> i32 {
    assert!(f(p).held.remove(&h).is_some(), "double native release {h}");
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
    f.epoch += 1;
    f.attached = true;
    0
}
unsafe extern "C" fn detach(p: *mut c_void) -> i32 {
    let f = f(p);
    f.attached = false;
    f.done.clear();
    0
}
unsafe extern "C" fn pool_info(p: *mut c_void, out: *mut QuicPoolInfo) -> i32 {
    let f = f(p);
    *out = QuicPoolInfo {
        size: core::mem::size_of::<QuicPoolInfo>() as u32,
        slot_bytes: 1704,
        headroom: 144,
        max_packet: 1448,
        capacity: 4,
        available: 4u32
            .saturating_sub(f.held.values().filter(|s| s.owned).count() as u32)
            .min(8u32.saturating_sub(
                f.held.len() as u32 + f.physical.len() as u32 + f.done.len() as u32,
            )),
    };
    0
}
unsafe extern "C" fn acquire(p: *mut c_void, reserve: u32, out: *mut u32) -> i32 {
    let mut info = QuicPoolInfo::default();
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
    ticket: *mut u32,
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
    let s = f.held.remove(&h).unwrap();
    *ticket = h;
    f.physical.push(Physical {
        slot: s,
        start: start as usize,
        len: len as usize,
        ticket: h,
        epoch: f.epoch,
    });
    0
}
unsafe extern "C" fn poll(p: *mut c_void, out: *mut QuicTxCompletion) -> i32 {
    let f = f(p);
    if f.done.is_empty() {
        return 0;
    }
    if f.held.values().filter(|s| s.owned).count() >= 4 {
        return ERR_BUSY;
    }
    let (mut done, mut slot) = f.done.remove(0);
    slot.owned = true;
    f.next += 1;
    done.packet = f.next;
    f.held.insert(f.next, slot);
    *out = done;
    1
}
static QUIC: Quic = Quic {
    version: QUIC_VERSION,
    size: core::mem::size_of::<Quic>() as u32,
    attach: Some(attach),
    detach: Some(detach),
    pool_info: Some(pool_info),
    acquire: Some(acquire),
    try_send: Some(transmit),
    poll_tx: Some(poll),
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
static LINK: Link = Link {
    version: LINK_VERSION,
    size: core::mem::size_of::<Link>() as u32,
    info: Some(link_info),
    ..unsafe { core::mem::zeroed() }
};
unsafe extern "C" fn component(_: *mut c_void, name: *const c_char) -> *const c_void {
    match CStr::from_ptr(name).to_bytes() {
        b"quic" => (&QUIC as *const Quic).cast(),
        b"link" => (&LINK as *const Link).cast(),
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
        id: 1, request_id: None };
    state.handle(hw, &call);
    if fake.held.get(&h).is_some_and(|s| !s.owned) {
        fake.held.remove(&h);
    }
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
        id: 0, request_id: None };
    state.handle(hw, &call);
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
fn two_stream_echo_uses_native_pool_and_busy_retains_exact_packets() {
    echo_fixture(false);
    echo_fixture(true);
}
fn echo_fixture(loss: bool) {
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
    let input = [vec![0x7e; 4096], vec![0x7d; 4096]];
    let mut sent = [0usize; 2];
    let mut received = [Vec::new(), Vec::new()];
    let mut ids = [0; 2];
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
            ids = [a.id(), b.id()];
            streams = Some([a, b]);
        }
        if let Some(streams) = streams.as_mut() {
            for i in 0..2 {
                if sent[i] < input[i].len() {
                    if let Ok(n) =
                        client.write_stream_and_finish(&mut streams[i], &input[i][sent[i]..])
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
        if received[0].len() == input[0].len() && received[1].len() == input[1].len() {
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
        "echo did not complete; errors {} last {} sent {:?}",
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
    let context_state = unsafe { &*(state.context as *const Context) };
    assert!(
        context_state.core.is_none(),
        "idle association must release the entire QUIC core and scoped pool"
    );
    assert_eq!(context_state.alarm, -1, "idle endpoint must stop its timer");
    assert!(fake.held.is_empty());
    assert!(fake.done.is_empty());
    assert!(fake.physical.is_empty());
    assert_eq!(
        fake.allocations.len(),
        1,
        "only small endpoint Context remains until stop"
    );
    state.stop(&hw);
    fake.complete_physical();
    assert!(fake.held.is_empty());
    assert!(fake.done.is_empty());
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
    let mut ticket = 0;
    assert_eq!(
        unsafe {
            QUIC.try_send.unwrap()(
                hw.ctx(),
                buffer.handle(),
                range.start as u32,
                5,
                ADDR_LINK + 1,
                1,
                &mut ticket,
            )
        },
        0
    );
    buffer.moved();
    state.stop(&hw);
    assert_eq!(state.context, 0);
    assert!(!fake.physical.is_empty());
    state.start(&hw).unwrap();
    assert_ne!(state.context, 0);
    assert_ne!(fake.epoch, 1);
    let _ = ptr;
    fake.complete_physical();
    assert!(
        fake.done.is_empty(),
        "old physical completion must not reach new endpoint"
    );
    drop(buffer);
    drop(pool);
    state.stop(&hw);
    assert!(fake.allocations.is_empty());
    assert!(fake.held.is_empty());
}
