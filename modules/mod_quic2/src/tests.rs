use super::*;
use core::ffi::{c_char, c_int, c_void, CStr};
use links::{serial_link_profile, tcp_link_profile, udp_link_profile, LINK_TCP_BASE};
use quic_base::{StaticPacketLease, StaticPacketPool};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, PacketPool, PoolPacket, QuicMux, RxMeta,
    RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId, TxStreamData,
};
use std::collections::{BTreeMap, VecDeque};
use std::{boxed::Box, vec, vec::Vec};
use xip_abi::{
    cbor, Call, Crypto, Host, Identity, PacketPoolInfo, Stream, StreamPacket, ABI_VERSION,
    ADDR_LINK, CALL_LINK, CRYPTO_VERSION, ERR_AGAIN, ERR_ARG, ERR_AUTH, ERR_BUSY, ERR_NOTFOUND,
    ERR_UNSUPPORTED, HOST_COMPONENT, HOST_START, HOST_STOP, IDENTITY_VERSION, PKT_HEADROOM,
    PKT_MAX, PKT_SLOT_MIN, SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE, SOCKET_EVENT_WRITE_DONE, STREAM_VERSION,
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
    hashes: BTreeMap<u32, xip_psa::Hasher>,
    next_hash: u32,
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

unsafe fn pkt_range_mut<'a>(f: &'a mut FakeHost, pkt: u32, offset: u32, len: u32, write: bool) -> Result<&'a mut [u8], i32> {
    let Some(s) = f.held.get_mut(&pkt) else {
        return Err(ERR_ARG);
    };
    if write && s.refs != 1 {
        return Err(ERR_BUSY);
    }
    let end = offset as usize + len as usize;
    if end > PKT_MAX as usize || s.start + end > s.bytes.len() {
        return Err(ERR_ARG);
    }
    Ok(&mut s.bytes[s.start + offset as usize..s.start + end])
}

unsafe extern "C" fn c_hash(u: *mut c_void, alg: u32, pkt: u32, off: u32, len: u32, out_pkt: u32, out_off: u32) -> c_int {
    let f = fh(u);
    let data = match pkt_range_mut(f, pkt, off, len, false) {
        Ok(d) => d.to_vec(),
        Err(e) => return e,
    };
    let Some(d) = xip_psa::hash(alg, &data) else {
        return ERR_UNSUPPORTED;
    };
    match pkt_range_mut(f, out_pkt, out_off, d.len() as u32, true) {
        Ok(out) => {
            out.copy_from_slice(&d);
            d.len() as c_int
        }
        Err(e) => e,
    }
}
unsafe extern "C" fn c_hash_start(u: *mut c_void, alg: u32) -> c_int {
    let f = fh(u);
    let Some(h) = xip_psa::Hasher::new(alg) else {
        return ERR_UNSUPPORTED;
    };
    f.next_hash += 1;
    let id = f.next_hash;
    f.hashes.insert(id, h);
    id as c_int
}
unsafe extern "C" fn c_hash_update(u: *mut c_void, handle: u32, pkt: u32, off: u32, len: u32) -> c_int {
    let f = fh(u);
    let data = match pkt_range_mut(f, pkt, off, len, false) {
        Ok(d) => d.to_vec(),
        Err(e) => return e,
    };
    match f.hashes.get_mut(&handle) {
        Some(h) => {
            h.update(&data);
            0
        }
        None => ERR_NOTFOUND,
    }
}
unsafe extern "C" fn c_hash_finish(u: *mut c_void, handle: u32, out_pkt: u32, out_off: u32) -> c_int {
    let f = fh(u);
    let Some(h) = f.hashes.remove(&handle) else {
        return ERR_NOTFOUND;
    };
    let d = h.finish();
    match pkt_range_mut(f, out_pkt, out_off, d.len() as u32, true) {
        Ok(out) => {
            out.copy_from_slice(&d);
            d.len() as c_int
        }
        Err(e) => e,
    }
}
unsafe extern "C" fn c_hash_abort(u: *mut c_void, handle: u32) -> c_int {
    fh(u).hashes.remove(&handle);
    0
}
unsafe extern "C" fn c_verify(u: *mut c_void, alg: u32, pkt: u32, off: u32, key_len: u32, msg_len: u32, sig_len: u32) -> c_int {
    let f = fh(u);
    if alg != xip_psa::ALG_ECDSA_SHA_256 {
        return ERR_UNSUPPORTED;
    }
    let total = key_len + msg_len + sig_len;
    let Ok(all) = pkt_range_mut(f, pkt, off, total, false) else {
        return ERR_ARG;
    };
    let (k, rest) = all.split_at(key_len as usize);
    let (m, s) = rest.split_at(msg_len as usize);
    if xip_psa::verify_p256_sha256(k, m, s) { 0 } else { ERR_AUTH }
}
unsafe extern "C" fn c_random(u: *mut c_void, pkt: u32, off: u32, len: u32) -> c_int {
    match pkt_range_mut(fh(u), pkt, off, len, true) {
        Ok(out) => {
            xip_psa::random(out);
            0
        }
        Err(e) => e,
    }
}
unsafe extern "C" fn c_aead_seal(u: *mut c_void, alg: u32, pkt: u32, off: u32, key_len: u32, aad_len: u32, text_len: u32) -> c_int {
    if alg != xip_psa::ALG_GCM && alg != xip_psa::ALG_CCM {
        return ERR_UNSUPPORTED;
    }
    if key_len != 16 && key_len != 32 {
        return ERR_ARG;
    }
    let hdr = key_len as usize + 12 + aad_len as usize;
    let total = hdr + text_len as usize + 16;
    let all = match pkt_range_mut(fh(u), pkt, off, total as u32, true) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let (head, rest) = all.split_at_mut(hdr);
    let (key, nonce_aad) = head.split_at(key_len as usize);
    let (nonce, aad) = nonce_aad.split_at(12);
    let nonce: &[u8; 12] = nonce.try_into().unwrap();
    let (body, tag_out) = rest.split_at_mut(text_len as usize);
    let Some(tag) = xip_psa::aead_seal_in_place(alg, key, nonce, aad, body) else {
        return ERR_ARG;
    };
    tag_out.copy_from_slice(&tag);
    (text_len + 16) as c_int
}
unsafe extern "C" fn c_aead_open(u: *mut c_void, alg: u32, pkt: u32, off: u32, key_len: u32, aad_len: u32, text_len: u32) -> c_int {
    if alg != xip_psa::ALG_GCM && alg != xip_psa::ALG_CCM {
        return ERR_UNSUPPORTED;
    }
    if (key_len != 16 && key_len != 32) || text_len < 16 {
        return ERR_ARG;
    }
    let hdr = key_len as usize + 12 + aad_len as usize;
    let total = hdr + text_len as usize;
    let all = match pkt_range_mut(fh(u), pkt, off, total as u32, true) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let (head, rest) = all.split_at_mut(hdr);
    let (key, nonce_aad) = head.split_at(key_len as usize);
    let (nonce, aad) = nonce_aad.split_at(12);
    let nonce: &[u8; 12] = nonce.try_into().unwrap();
    let plain_len = text_len as usize - 16;
    let (body, tag_in) = rest.split_at_mut(plain_len);
    let tag: [u8; 16] = tag_in.try_into().unwrap();
    if xip_psa::aead_open_in_place(alg, key, nonce, aad, body, &tag) {
        plain_len as c_int
    } else {
        ERR_AUTH
    }
}
unsafe extern "C" fn c_aead_pkt(
    u: *mut c_void,
    spec: u32,
    src_pkt: u32,
    src_off: u32,
    dst_pkt: u32,
    dst_off: u32,
    text_len: u32,
) -> c_int {
    let f = fh(u);
    let aad_len = (spec & 0xff) as usize;
    let key_len = ((spec >> 8) & 0xff) as usize;
    let alg = match (spec >> 16) & 0xff {
        0 => xip_psa::ALG_GCM,
        1 => xip_psa::ALG_CCM,
        _ => return ERR_UNSUPPORTED,
    };
    let is_open = (spec & xip_abi::AEAD_PKT_FLAG_OPEN) != 0;
    if (key_len != 16 && key_len != 32) || (is_open && text_len < 16) || src_pkt == dst_pkt {
        return ERR_ARG;
    }
    let hdr_len = key_len + 12 + aad_len;
    if (dst_off as usize) < hdr_len {
        return ERR_ARG;
    }
    let out_len = if is_open { text_len as usize - 16 } else { text_len as usize + 16 };
    let src_bytes = {
        let Some(src) = f.held.get(&src_pkt) else { return ERR_ARG };
        let Some(end) = (src_off as usize).checked_add(text_len as usize) else { return ERR_ARG };
        if end > src.bytes.len() { return ERR_ARG; }
        src.bytes[src_off as usize..end].to_vec()
    };
    let Some(dst) = f.held.get_mut(&dst_pkt) else { return ERR_ARG };
    if dst.refs != 1 {
        return ERR_BUSY;
    }
    let Some(dst_end) = (dst_off as usize).checked_add(out_len) else { return ERR_ARG };
    if dst_end > dst.bytes.len() {
        return ERR_ARG;
    }
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    let mut aad = [0u8; 255];
    key[..key_len].copy_from_slice(&dst.bytes[..key_len]);
    nonce.copy_from_slice(&dst.bytes[key_len..key_len + 12]);
    if aad_len > 0 {
        aad[..aad_len].copy_from_slice(&dst.bytes[key_len + 12..hdr_len]);
    }
    dst.bytes[..hdr_len].fill(0);
    if !is_open {
        let plain_len = text_len as usize;
        dst.bytes[dst_off as usize..dst_off as usize + plain_len].copy_from_slice(&src_bytes);
        let body = &mut dst.bytes[dst_off as usize..dst_off as usize + plain_len];
        let Some(tag) = xip_psa::aead_seal_in_place(alg, &key[..key_len], &nonce, &aad[..aad_len], body) else {
            return ERR_ARG;
        };
        dst.bytes[dst_off as usize + plain_len..dst_end].copy_from_slice(&tag);
        out_len as c_int
    } else {
        let plain_len = out_len;
        dst.bytes[dst_off as usize..dst_end].copy_from_slice(&src_bytes[..plain_len]);
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&src_bytes[plain_len..]);
        let body = &mut dst.bytes[dst_off as usize..dst_end];
        if xip_psa::aead_open_in_place(alg, &key[..key_len], &nonce, &aad[..aad_len], body, &tag) {
            plain_len as c_int
        } else {
            ERR_AUTH
        }
    }
}

static CRYPTO_TABLE: Crypto = Crypto {
    version: CRYPTO_VERSION,
    size: core::mem::size_of::<Crypto>() as u32,
    hash: Some(c_hash),
    hash_start: Some(c_hash_start),
    hash_update: Some(c_hash_update),
    hash_finish: Some(c_hash_finish),
    hash_abort: Some(c_hash_abort),
    verify: Some(c_verify),
    random: Some(c_random),
    aead_seal: Some(c_aead_seal),
    aead_open: Some(c_aead_open),
    aead_pkt: Some(c_aead_pkt),
};

unsafe extern "C" fn id_public_key(u: *mut c_void, pkt: u32, off: u32) -> c_int {
    let pk = xip_psa::identity_public_key();
    match pkt_range_mut(fh(u), pkt, off, 65, true) {
        Ok(out) => {
            out.copy_from_slice(&pk);
            0
        }
        Err(e) => e,
    }
}
unsafe extern "C" fn id_sign(u: *mut c_void, pkt: u32, off: u32, len: u32, out_pkt: u32, out_off: u32) -> c_int {
    let f = fh(u);
    let msg = match pkt_range_mut(f, pkt, off, len, false) {
        Ok(m) => m.to_vec(),
        Err(e) => return e,
    };
    let sig = xip_psa::identity_sign(&msg);
    match pkt_range_mut(f, out_pkt, out_off, 64, true) {
        Ok(out) => {
            out.copy_from_slice(&sig);
            0
        }
        Err(e) => e,
    }
}

static IDENTITY_TABLE: Identity = Identity {
    version: IDENTITY_VERSION,
    size: core::mem::size_of::<Identity>() as u32,
    public_key: Some(id_public_key),
    sign: Some(id_sign),
};

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
    match CStr::from_ptr(name).to_bytes() {
        b"stream" => (&STREAM_TABLE as *const Stream).cast(),
        b"crypto" => (&CRYPTO_TABLE as *const Crypto).cast(),
        b"identity" => (&IDENTITY_TABLE as *const Identity).cast(),
        _ => core::ptr::null(),
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

#[test]
fn quic2_resets_idle_association_for_fresh_client_pn1_epoch() {
    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let mut state = State::default();
    assert_eq!(call_host(&mut state, &host, HOST_START), 0);

    let tick_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: TICK,
        ..unsafe { core::mem::zeroed() }
    };

    // First client session at pn=1, StreamId(0).
    let pool1: StaticPacketPool<32> = StaticPacketPool::new();
    let mut c1_mux = QuicMux::new(&pool1, embedded_mux_limits());
    c1_mux.add_association(OWNER_ASSOC, true).unwrap();
    let mut c1_assoc: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(default_assoc_config());
    c1_assoc.upsert_link(LINK_SERIAL, serial_link_profile());
    let s1 = c1_mux.open_stream(OWNER_ASSOC, true).unwrap();
    c1_mux
        .write_stream(OWNER_ASSOC, s1, TxStreamData::Bytes(b"first-req"), true, &mut c1_assoc)
        .unwrap();
    let pkt1 = c1_assoc.pop_outbound_packet().unwrap();
    assert_eq!(
        call_link(&mut state, &host, &mut fake, 0, ADDR_LINK + 1, QUIC2_SERIAL_PORT, pkt1.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&1].received, b"first-req");
    fake.queue_consumer_reply(1, b"first-resp", true);
    dispatch(&host, &mut state, &tick_call);
    assert!(!fake.link_tx.is_empty());
    fake.link_tx.clear();

    // Fresh client session restarts at pn=1, StreamId(0) without calling quic2.reset.
    let pool2: StaticPacketPool<32> = StaticPacketPool::new();
    let mut c2_mux = QuicMux::new(&pool2, embedded_mux_limits());
    c2_mux.add_association(OWNER_ASSOC, true).unwrap();
    let mut c2_assoc: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(default_assoc_config());
    c2_assoc.upsert_link(LINK_SERIAL, serial_link_profile());
    let s2 = c2_mux.open_stream(OWNER_ASSOC, true).unwrap();
    c2_mux
        .write_stream(OWNER_ASSOC, s2, TxStreamData::Bytes(b"second-req"), true, &mut c2_assoc)
        .unwrap();
    let pkt2 = c2_assoc.pop_outbound_packet().unwrap();
    assert_eq!(
        call_link(&mut state, &host, &mut fake, 0, ADDR_LINK + 1, QUIC2_SERIAL_PORT, pkt2.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&2].received, b"second-req");
    fake.queue_consumer_reply(2, b"second-resp", true);
    dispatch(&host, &mut state, &tick_call);
    assert!(!fake.link_tx.is_empty());
    fake.link_tx.clear();

    assert_eq!(call_host(&mut state, &host, HOST_STOP), 0);
    assert!(fake.held.is_empty(), "leaked packets: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty());
}

fn client_handshake_config(client_cid: u64) -> AssocConfig {
    let cid = ConnectionId::new(client_cid).unwrap();
    AssocConfig {
        assoc_id: OWNER_ASSOC,
        local_cid: Some(cid),
        default_peer_cid: ConnectionId::new(1).unwrap(),
        handshake_mode: HandshakeMode::Client,
        local_params: TransportParameters {
            initial_scid: Some(cid),
            initial_max_data: Some(64 * 1024),
            initial_max_stream_data_bidi_local: Some(32 * 1024),
            initial_max_stream_data_bidi_remote: Some(32 * 1024),
            initial_max_streams_bidi: Some(8),
            initial_max_streams_uni: Some(8),
            public_key: Some([0x02; PUBLIC_KEY_COMPRESSED_LEN]),
            ..Default::default()
        },
        ..default_assoc_config()
    }
}

#[test]
fn initial_handshake_and_coalesced_stream_on_esp32() {
    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let mut state = State::default();
    assert_eq!(call_host(&mut state, &host, HOST_START), 0);

    let tick_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: TICK,
        ..unsafe { core::mem::zeroed() }
    };

    let client_cid = ConnectionId::new(7).unwrap();
    let pool: StaticPacketPool<32> = StaticPacketPool::new();
    let mut c_mux = QuicMux::new(&pool, embedded_mux_limits());
    c_mux.add_association(OWNER_ASSOC, true).unwrap();
    let mut c_assoc: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(client_handshake_config(7));
    c_assoc.upsert_link(LINK_UDP, udp_link_profile());

    // 1. Client writes STREAM(0) immediately (1-RTT coalesced Initial + CRYPTO + STREAM).
    let s1 = c_mux.open_stream(OWNER_ASSOC, true).unwrap();
    c_mux
        .write_stream(OWNER_ASSOC, s1, TxStreamData::Bytes(b"esp-init-req"), true, &mut c_assoc)
        .unwrap();
    let pkt1 = c_assoc.pop_outbound_packet().unwrap();
    assert!(is_initial_packet(pkt1.packet.bytes()));
    c_assoc.on_driver_complete(LINK_UDP, pkt1.packet_number, quic_link::LinkTxOutcome::Sent, 1_000);
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x101, pkt1.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&1].received, b"esp-init-req");
    assert!(fake.consumers[&1].fin);

    // 2. mod_quic2 emitted Server Initial immediately on call_link's tick, and sends the
    //    consumer's ShortHeader STREAM reply on the next tick.
    fake.queue_consumer_reply(1, b"esp-init-resp", true);
    fake.now_us = 2_000;
    dispatch(&host, &mut state, &tick_call);
    assert!(!fake.link_tx.is_empty());

    let mut c_rx = ClientRx::default();
    let mut saw_server_initial = false;
    let mut server_cid = None;
    for (link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(link, ADDR_LINK + 2);
        assert_eq!(peer, 0x101);
        if is_initial_packet(&wire) {
            saw_server_initial = true;
            let (init_hdr, _) = InitialHeader::decode(&wire).unwrap();
            assert_eq!(init_hdr.dcid, Some(client_cid));
            server_cid = init_hdr.scid;
        } else {
            let (sh, _) = ShortHeader::decode(&wire).unwrap();
            assert_eq!(sh.dcid, client_cid);
        }
        let mut p = (&pool).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = c_assoc.receive_packet(LINK_UDP, p, 3_000).unwrap();
        if demux.is_initial {
            assert_eq!(
                demux.peer_params.and_then(|tp| tp.max_udp_payload_size),
                Some(1472)
            );
        }
        if let Some(mux_pkt) = demux.mux_packet {
            c_mux
                .receive_packet(
                    RxMeta {
                        association: OWNER_ASSOC,
                        received_at_us: 3_000,
                    },
                    mux_pkt,
                    &mut c_assoc,
                    &mut c_rx,
                )
                .unwrap();
        }
    }
    assert!(saw_server_initial);
    assert_eq!(c_rx.streams[&s1].0, b"esp-init-resp");
    assert!(c_rx.streams[&s1].1);
    assert!(c_assoc.is_handshake_complete());
    let server_cid = server_cid.expect("server allocated dynamic CID >= 2");
    assert!(server_cid.value() >= 2);

    // 3. Subsequent Client stream uses compact ShortHeader targeting `dcid = server_cid`.
    let s2 = c_mux.open_stream(OWNER_ASSOC, true).unwrap();
    c_mux
        .write_stream(OWNER_ASSOC, s2, TxStreamData::Bytes(b"esp-short-req"), true, &mut c_assoc)
        .unwrap();
    let pkt2 = c_assoc.pop_outbound_packet().unwrap();
    assert!(!is_initial_packet(pkt2.packet.bytes()));
    let (sh, _) = ShortHeader::decode(pkt2.packet.bytes()).unwrap();
    assert_eq!(sh.dcid, server_cid);
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x101, pkt2.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&2].received, b"esp-short-req");
    fake.queue_consumer_reply(2, b"esp-short-resp", true);
    fake.now_us = 4_000;
    dispatch(&host, &mut state, &tick_call);
    assert!(!fake.link_tx.is_empty());
    fake.link_tx.clear();

    assert_eq!(call_host(&mut state, &host, HOST_STOP), 0);
    assert!(fake.held.is_empty(), "leaked packets: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty());
}

fn status_errors(state: &mut State, host: &Host, fake: &mut FakeHost) -> i64 {
    let status_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 1,
        flags: 1,
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(host, state, &status_call);
    let reply = fake.replies.pop().unwrap();
    cbor::get_int(&reply, 7).unwrap().unwrap()
}

#[test]
fn single_owner_assoc_rejects_when_busy_and_replaces_when_idle() {
    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let mut state = State::default();
    assert_eq!(call_host(&mut state, &host, HOST_START), 0);

    let tick_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: TICK,
        ..unsafe { core::mem::zeroed() }
    };

    // Owner Client A (`client_cid = 7`, route token `0x101`) opens a stream on slot 0.
    let pool_a: StaticPacketPool<32> = StaticPacketPool::new();
    let mut mux_a = QuicMux::new(&pool_a, embedded_mux_limits());
    mux_a.add_association(OWNER_ASSOC, true).unwrap();
    let mut assoc_a: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(client_handshake_config(7));
    assoc_a.upsert_link(LINK_UDP, udp_link_profile());
    let sa = mux_a.open_stream(OWNER_ASSOC, true).unwrap();
    mux_a
        .write_stream(OWNER_ASSOC, sa, TxStreamData::Bytes(b"peer-a"), true, &mut assoc_a)
        .unwrap();
    let pa = assoc_a.pop_outbound_packet().unwrap();
    fake.now_us = 1_000;
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x101, pa.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&1].received, b"peer-a");

    // Client B (`client_cid = 9`, route token `0x202`) arrives while Client A still has an open stream -> ERR_BUSY.
    let pool_b: StaticPacketPool<32> = StaticPacketPool::new();
    let mut mux_b = QuicMux::new(&pool_b, embedded_mux_limits());
    mux_b.add_association(OWNER_ASSOC, true).unwrap();
    let mut assoc_b: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(client_handshake_config(9));
    assoc_b.upsert_link(LINK_UDP, udp_link_profile());
    let sb = mux_b.open_stream(OWNER_ASSOC, true).unwrap();
    mux_b
        .write_stream(OWNER_ASSOC, sb, TxStreamData::Bytes(b"peer-b"), true, &mut assoc_b)
        .unwrap();
    let pb = assoc_b.pop_outbound_packet().unwrap();
    fake.now_us = 2_000;
    let _ = call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x202, pb.packet.bytes());
    assert_eq!(fake.consumers.len(), 1);
    assert_eq!(status_errors(&mut state, &host, &mut fake), 1);

    // Finish Client A's stream & deliver its reply so single-owner slot 0 becomes idle.
    fake.queue_consumer_reply(1, b"reply-a", true);
    fake.now_us = 3_000;
    dispatch(&host, &mut state, &tick_call);
    let mut c_rx_a = ClientRx::default();
    for (_link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(peer, 0x101);
        let mut p = (&pool_a).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = assoc_a.receive_packet(LINK_UDP, p, 3_500).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            mux_a
                .receive_packet(
                    RxMeta {
                        association: OWNER_ASSOC,
                        received_at_us: 3_500,
                    },
                    mux_pkt,
                    &mut assoc_a,
                    &mut c_rx_a,
                )
                .unwrap();
        }
    }
    assert_eq!(c_rx_a.streams[&sa].0, b"reply-a");
    let mut pool_a_ref = &pool_a;
    let _ = assoc_a.poll_timers(&mut pool_a_ref, 4_000);
    while let Some(ack_a) = assoc_a.pop_outbound_packet() {
        fake.now_us = 4_000;
        assert_eq!(
            call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x101, ack_a.packet.bytes()),
            0
        );
    }

    // Now Client B retries once slot 0 is idle -> single-owner slot replaces cleanly!
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x202, pb.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&2].received, b"peer-b");
    fake.queue_consumer_reply(2, b"reply-b", true);
    fake.now_us = 6_000;
    dispatch(&host, &mut state, &tick_call);
    let mut got_b_initial = false;
    let mut got_b_stream = false;
    for (_link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(peer, 0x202);
        if is_initial_packet(&wire) {
            assert_eq!(InitialHeader::decode(&wire).unwrap().0.dcid, ConnectionId::new(9));
            got_b_initial = true;
        } else {
            assert_eq!(ShortHeader::decode(&wire).unwrap().0.dcid, ConnectionId::new(9).unwrap());
            got_b_stream = true;
        }
    }
    assert!(got_b_initial && got_b_stream);

    assert_eq!(call_host(&mut state, &host, HOST_STOP), 0);
    assert!(fake.held.is_empty(), "leaked packets: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty());
}

#[test]
fn phase_a_psa_crypto_and_per_link_optional_encryption_on_esp32() {
    use quic_link::{
        derive_device_secret_with, derive_peer_api_key_with, vip6_from_public_key_with,
        FLAG_PROTECTED,
    };

    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let hw = unsafe { Hw::new(&host) }.unwrap();
    let psa_provider = psa_crypto_provider(hw);
    let mut state = State::default();
    assert_eq!(call_host(&mut state, &host, HOST_START), 0);

    let esp_pub = [0x03u8; PUBLIC_KEY_COMPRESSED_LEN];
    let owner_pub = [0x02u8; PUBLIC_KEY_COMPRESSED_LEN];
    let node_secret = derive_device_secret_with(
        &psa_provider,
        b"control-plane-root-secret-material",
        &esp_pub,
    )
    .unwrap();
    state.configure_crypto(Some(esp_pub), Some(node_secret), None);

    let tick_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: TICK,
        ..unsafe { core::mem::zeroed() }
    };

    // Part 1: PPP serial (`LINK_SERIAL`) must NEVER encrypt even when `node_secret` is configured!
    let pool0: StaticPacketPool<32> = StaticPacketPool::new();
    let mut m0 = QuicMux::new(&pool0, embedded_mux_limits());
    m0.add_association(OWNER_ASSOC, true).unwrap();
    let mut a0: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(default_assoc_config());
    a0.upsert_link(LINK_SERIAL, serial_link_profile());
    let s0 = m0.open_stream(OWNER_ASSOC, true).unwrap();
    let serial_msg = b"ppp-serial-stays-unencrypted";
    m0.write_stream(OWNER_ASSOC, s0, TxStreamData::Bytes(serial_msg), true, &mut a0)
        .unwrap();
    let pkt0 = a0.pop_outbound_packet().unwrap();
    assert_eq!(pkt0.packet.bytes()[0] & FLAG_PROTECTED, 0);
    assert_eq!(
        call_link(&mut state, &host, &mut fake, 0, ADDR_LINK + 1, QUIC2_SERIAL_PORT, pkt0.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&1].received, serial_msg);
    fake.queue_consumer_reply(1, b"ppp-reply-cleartext", true);
    fake.now_us = 1_500;
    dispatch(&host, &mut state, &tick_call);
    let mut rx0 = ClientRx::default();
    let mut saw_ppp_reply = false;
    for (_link, _peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(wire[0] & FLAG_PROTECTED, 0);
        if wire
            .windows(b"ppp-reply-cleartext".len())
            .any(|w| w == b"ppp-reply-cleartext")
        {
            saw_ppp_reply = true;
        }
        let mut p = (&pool0).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = a0.receive_packet(LINK_SERIAL, p, 1_800).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            m0.receive_packet(
                RxMeta { association: OWNER_ASSOC, received_at_us: 1_800 },
                mux_pkt,
                &mut a0,
                &mut rx0,
            )
            .unwrap();
        }
    }
    assert!(saw_ppp_reply);
    assert_eq!(rx0.streams[&s0].0, b"ppp-reply-cleartext");

    // Part 2: Pre-keyed ShortHeader-only PSA AES-128-GCM over Wi-Fi/UDP (`LINK_UDP`, `encrypt: true`).
    let owner_api_key = derive_peer_api_key_with(&psa_provider, &node_secret, &[]).unwrap();
    let pool1: StaticPacketPool<32> = StaticPacketPool::new();
    let mut m1 = QuicMux::new(&pool1, embedded_mux_limits());
    m1.add_association(OWNER_ASSOC, true).unwrap();
    let mut a1: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(default_assoc_config());
    a1.set_crypto_config(AssocCryptoConfig {
        provider: psa_provider,
        role: CryptoRole::Client,
        local_public_key: None,
        local_random: [0; 32],
        send_api_key: Some(owner_api_key),
        node_secret: None,
    })
    .unwrap();
    a1.upsert_link(LINK_UDP, udp_link_profile());

    let s1 = m1.open_stream(OWNER_ASSOC, true).unwrap();
    let req_bytes = b"esp32-psa-prekeyed-udp-req";
    m1.write_stream(OWNER_ASSOC, s1, TxStreamData::Bytes(req_bytes), true, &mut a1)
        .unwrap();
    let pkt1 = a1.pop_outbound_packet().unwrap();
    assert!(!is_initial_packet(pkt1.packet.bytes()));
    assert_eq!(pkt1.packet.bytes()[0] & FLAG_PROTECTED, FLAG_PROTECTED);
    assert!(!pkt1.packet.bytes().windows(req_bytes.len()).any(|w| w == req_bytes));
    fake.now_us = 2_000;
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x333, pkt1.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&2].received, req_bytes);
    fake.queue_consumer_reply(2, b"esp32-psa-prekeyed-resp", true);
    fake.now_us = 2_500;
    dispatch(&host, &mut state, &tick_call);

    let mut rx1 = ClientRx::default();
    for (_link, _peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert!(!is_initial_packet(&wire));
        assert_eq!(wire[0] & FLAG_PROTECTED, FLAG_PROTECTED);
        let mut p = (&pool1).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = a1.receive_packet(LINK_UDP, p, 2_800).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            m1.receive_packet(
                RxMeta {
                    association: OWNER_ASSOC,
                    received_at_us: 2_800,
                },
                mux_pkt,
                &mut a1,
                &mut rx1,
            )
            .unwrap();
        }
    }
    assert_eq!(rx1.streams[&s1].0, b"esp32-psa-prekeyed-resp");
    assert!(rx1.streams[&s1].1);

    let mut pool1_ref = &pool1;
    let _ = a1.poll_timers(&mut pool1_ref, 3_000);
    while let Some(ack_pkt) = a1.pop_outbound_packet() {
        assert_eq!(ack_pkt.packet.bytes()[0] & FLAG_PROTECTED, FLAG_PROTECTED);
        assert_eq!(
            call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x333, ack_pkt.packet.bytes()),
            0
        );
    }

    // Part 3: Wrong verifier in Initial is rejected without opening any stream.
    let mut bad_cfg = client_handshake_config(21);
    bad_cfg.local_params.public_key = Some(owner_pub);
    let pool_bad: StaticPacketPool<32> = StaticPacketPool::new();
    let mut a_bad: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(bad_cfg);
    a_bad
        .set_crypto_config(AssocCryptoConfig {
            provider: psa_provider,
            role: CryptoRole::Client,
            local_public_key: Some(owner_pub),
            local_random: [0x11; 32],
            send_api_key: Some([0x99; 32]),
            node_secret: None,
        })
        .unwrap();
    a_bad.upsert_link(LINK_UDP, udp_link_profile());
    let mut bad_pool_ref = &pool_bad;
    a_bad.enqueue_initial(&mut bad_pool_ref).unwrap();
    let p_bad = a_bad.pop_outbound_packet().unwrap();
    fake.now_us = 4_000;
    let _ = call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x444, p_bad.packet.bytes());
    assert!(status_errors(&mut state, &host, &mut fake) >= 1);
    assert_eq!(fake.consumers.len(), 2);

    // Part 4: Valid per-caller api_key authenticates Initial, derives vip6, and encrypts ShortHeader via PSA.
    let caller_api_key =
        derive_peer_api_key_with(&psa_provider, &node_secret, &owner_pub).unwrap();
    let pool2: StaticPacketPool<32> = StaticPacketPool::new();
    let mut m2 = QuicMux::new(&pool2, embedded_mux_limits());
    m2.add_association(OWNER_ASSOC, true).unwrap();
    let mut a2: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(client_handshake_config(22));
    a2.set_crypto_config(AssocCryptoConfig {
        provider: psa_provider,
        role: CryptoRole::Client,
        local_public_key: Some(owner_pub),
        local_random: [0x22; 32],
        send_api_key: Some(caller_api_key),
        node_secret: None,
    })
    .unwrap();
    a2.upsert_link(LINK_UDP, udp_link_profile());
    let s2 = m2.open_stream(OWNER_ASSOC, true).unwrap();
    m2.write_stream(OWNER_ASSOC, s2, TxStreamData::Bytes(b"esp32-auth-init"), true, &mut a2)
        .unwrap();
    let p2 = a2.pop_outbound_packet().unwrap();
    assert!(is_initial_packet(p2.packet.bytes()));
    fake.now_us = 5_000;
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x555, p2.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&3].received, b"esp32-auth-init");

    fake.queue_consumer_reply(3, b"esp32-auth-reply", true);
    fake.now_us = 6_000;
    dispatch(&host, &mut state, &tick_call);
    let mut rx2 = ClientRx::default();
    let mut saw_short_enc = false;
    for (_link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(peer, 0x555);
        if !is_initial_packet(&wire) {
            assert_eq!(wire[0] & FLAG_PROTECTED, FLAG_PROTECTED);
            saw_short_enc = true;
        }
        let mut p = (&pool2).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = a2.receive_packet(LINK_UDP, p, 6_500).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            m2.receive_packet(
                RxMeta {
                    association: OWNER_ASSOC,
                    received_at_us: 6_500,
                },
                mux_pkt,
                &mut a2,
                &mut rx2,
            )
            .unwrap();
        }
    }
    assert!(saw_short_enc);
    assert!(a2.is_handshake_complete());
    assert_eq!(
        a2.peer_vip6(),
        Some(vip6_from_public_key_with(&psa_provider, &esp_pub).unwrap())
    );
    assert_eq!(rx2.streams[&s2].0, b"esp32-auth-reply");

    // Part 5: Configure ESP32 Wi-Fi/NOW (`LINK_UDP`) NOT to encrypt via CBOR method 3 (`config`) for testing!
    state.set_udp_encryption(true);
    let mut cfg_req = [0u8; 16];
    let mut cw = Writer::new(&mut cfg_req);
    cw.map(1).uint(1).boolean(false);
    let cfg_bytes = cw.finish().unwrap();
    let cfg_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 3,
        flags: 1,
        data: cfg_bytes.as_ptr(),
        len: cfg_bytes.len(),
        ..unsafe { core::mem::zeroed() }
    };
    fake.replies.clear();
    assert_eq!(dispatch(&host, &mut state, &cfg_call), 0);
    let cfg_reply = fake.replies.pop().unwrap();
    assert_eq!(
        cbor::get(&cfg_reply, 1).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Bool(false))
    );

    // After resetting active streams, `LINK_UDP` sends/accepts cleartext even though `node_secret` is set!
    let reset_req = [0xa0u8];
    let reset_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 2,
        flags: 1,
        data: reset_req.as_ptr(),
        len: 1,
        ..unsafe { core::mem::zeroed() }
    };
    fake.replies.clear();
    assert_eq!(dispatch(&host, &mut state, &reset_call), 0);
    fake.replies.clear();

    let pool3: StaticPacketPool<32> = StaticPacketPool::new();
    let mut m3 = QuicMux::new(&pool3, embedded_mux_limits());
    m3.add_association(OWNER_ASSOC, true).unwrap();
    let mut a3: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(default_assoc_config());
    a3.upsert_link(LINK_UDP, udp_link_profile().with_encryption(false));
    let s3 = m3.open_stream(OWNER_ASSOC, true).unwrap();
    let test_plain = b"wifi-udp-unencrypted-for-testing";
    m3.write_stream(OWNER_ASSOC, s3, TxStreamData::Bytes(test_plain), true, &mut a3)
        .unwrap();
    let pkt3 = a3.pop_outbound_packet().unwrap();
    assert_eq!(pkt3.packet.bytes()[0] & FLAG_PROTECTED, 0);
    fake.now_us = 8_000;
    assert_eq!(
        call_link(&mut state, &host, &mut fake, RX, ADDR_LINK + 2, 0x666, pkt3.packet.bytes()),
        0
    );
    assert_eq!(fake.consumers[&4].received, test_plain);
    fake.queue_consumer_reply(4, b"udp-test-plain-reply", true);
    fake.now_us = 8_500;
    dispatch(&host, &mut state, &tick_call);
    let mut rx3 = ClientRx::default();
    let mut saw_udp_plain_reply = false;
    for (_link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(peer, 0x666);
        assert_eq!(wire[0] & FLAG_PROTECTED, 0);
        if wire
            .windows(b"udp-test-plain-reply".len())
            .any(|w| w == b"udp-test-plain-reply")
        {
            saw_udp_plain_reply = true;
        }
        let mut p = (&pool3).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = a3.receive_packet(LINK_UDP, p, 8_800).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            m3.receive_packet(
                RxMeta { association: OWNER_ASSOC, received_at_us: 8_800 },
                mux_pkt,
                &mut a3,
                &mut rx3,
            )
            .unwrap();
        }
    }
    assert!(saw_udp_plain_reply);
    assert_eq!(rx3.streams[&s3].0, b"udp-test-plain-reply");

    // Part 6: Verify unified link MTU (PPP == UDP == TCP == 1472) and configurable PPP/TCP crypto via method 3.
    assert_eq!(links::SERIAL_FRAME_MTU, links::UDP_FRAME_MTU);
    assert_eq!(links::TCP_FRAME_MTU, links::UDP_FRAME_MTU);
    assert_eq!(links::serial_link_profile().mtu, links::udp_link_profile().mtu);
    assert_eq!(links::tcp_link_profile().mtu, links::udp_link_profile().mtu);
    assert!(!links::serial_link_profile().encrypt);
    assert!(!links::tcp_link_profile().encrypt);
    assert!(links::udp_link_profile().encrypt);

    let mut cfg_ppp_buf = [0u8; 24];
    let mut cw2 = Writer::new(&mut cfg_ppp_buf);
    cw2.map(2).uint(4).boolean(true).uint(5).boolean(true);
    let cfg_ppp_bytes = cw2.finish().unwrap();
    let cfg_ppp_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 3,
        flags: 1,
        data: cfg_ppp_bytes.as_ptr(),
        len: cfg_ppp_bytes.len(),
        ..unsafe { core::mem::zeroed() }
    };
    fake.replies.clear();
    assert_eq!(dispatch(&host, &mut state, &cfg_ppp_call), 0);
    let cfg_ppp_reply = fake.replies.pop().unwrap();
    assert_eq!(
        cbor::get(&cfg_ppp_reply, 3).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Bool(true))
    );
    assert_eq!(
        cbor::get(&cfg_ppp_reply, 4).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Bool(true))
    );
    fake.replies.clear();
    assert_eq!(dispatch(&host, &mut state, &reset_call), 0);
    fake.replies.clear();

    let pool4: StaticPacketPool<32> = StaticPacketPool::new();
    let mut m4 = QuicMux::new(&pool4, embedded_mux_limits());
    m4.add_association(OWNER_ASSOC, true).unwrap();
    let mut a4: QuicLinkAssoc<StaticPacketLease<'_, 32>> =
        QuicLinkAssoc::new(default_assoc_config());
    a4.set_crypto_config(AssocCryptoConfig {
        provider: psa_provider,
        role: CryptoRole::Client,
        local_public_key: None,
        local_random: [0x33; 32],
        send_api_key: Some(owner_api_key),
        node_secret: None,
    })
    .unwrap();
    a4.upsert_link(LINK_SERIAL, serial_link_profile_with_encrypt(true));
    let s4 = m4.open_stream(OWNER_ASSOC, true).unwrap();
    let ppp_secret_msg = b"ppp-serial-encrypted-for-testing";
    m4.write_stream(
        OWNER_ASSOC,
        s4,
        TxStreamData::Bytes(ppp_secret_msg),
        true,
        &mut a4,
    )
    .unwrap();
    let pkt4 = a4.pop_outbound_packet().unwrap();
    assert_eq!(pkt4.packet.bytes()[0] & FLAG_PROTECTED, FLAG_PROTECTED);
    assert!(!pkt4
        .packet
        .bytes()
        .windows(ppp_secret_msg.len())
        .any(|w| w == ppp_secret_msg));
    fake.now_us = 10_000;
    assert_eq!(
        call_link(
            &mut state,
            &host,
            &mut fake,
            0,
            ADDR_LINK + 1,
            QUIC2_SERIAL_PORT,
            pkt4.packet.bytes()
        ),
        0
    );
    assert_eq!(fake.consumers[&5].received, ppp_secret_msg);
    fake.queue_consumer_reply(5, b"ppp-enc-reply", true);
    fake.now_us = 10_500;
    dispatch(&host, &mut state, &tick_call);
    let mut rx4 = ClientRx::default();
    let mut saw_ppp_enc_reply = false;
    for (_link, peer, wire) in core::mem::take(&mut fake.link_tx) {
        assert_eq!(peer, QUIC2_SERIAL_PORT);
        assert_eq!(wire[0] & FLAG_PROTECTED, FLAG_PROTECTED);
        let mut p = (&pool4).alloc().unwrap();
        p.append(&wire).unwrap();
        let demux = a4.receive_packet(LINK_SERIAL, p, 10_800).unwrap();
        if let Some(mux_pkt) = demux.mux_packet {
            m4.receive_packet(
                RxMeta {
                    association: OWNER_ASSOC,
                    received_at_us: 10_800,
                },
                mux_pkt,
                &mut a4,
                &mut rx4,
            )
            .unwrap();
        }
        saw_ppp_enc_reply = true;
    }
    assert!(saw_ppp_enc_reply);
    assert_eq!(rx4.streams[&s4].0, b"ppp-enc-reply");

    // Verify `set_tcp_encryption` and restore default per-link encryption flags.
    state.set_tcp_encryption(true);
    state.set_tcp_encryption(false);
    state.set_serial_encryption(false);
    state.set_udp_encryption(true);

    assert_eq!(call_host(&mut state, &host, HOST_STOP), 0);
    assert!(fake.held.is_empty(), "leaked packets: {:?}", fake.held.keys());
    assert!(fake.allocations.is_empty());
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct BenchTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
fn bench_thread_cpu_ns() -> u64 {
    const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
    unsafe extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut BenchTimespec) -> i32;
    }
    let mut ts = BenchTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    } else {
        0
    }
}

#[cfg(not(target_os = "linux"))]
fn bench_thread_cpu_ns() -> u64 {
    0
}

#[test]
fn packet_borrow_gcm_and_ccm_roundtrip_and_config_tag6() {
    use quic_base::{open_short_packet_in_place, seal_short_packet_in_place_with_pn, DirectionKeyMaterial, ProviderAeadKey};
    use quic_mux::PacketPool as _;

    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let hw = unsafe { Hw::new(&host) }.unwrap();
    let pool = XipPool::new(hw);
    let cid = ConnectionId::new(1).unwrap();

    for (cipher, expected_alg_provider) in [
        (AeadCipher::Gcm, psa_crypto_provider_for(hw, AeadCipher::Gcm)),
        (AeadCipher::Ccm, psa_crypto_provider_for(hw, AeadCipher::Ccm)),
    ] {
        assert!(expected_alg_provider.aead_packet.is_some());
        let pkey = ProviderAeadKey::new(
            expected_alg_provider,
            DirectionKeyMaterial::from_material(&[0x33u8; 28]),
        );
        let mut pkt = pool.alloc().unwrap();
        // Verify TX packet start is 64-byte aligned inside native slot.
        assert_eq!(pkt.range().start, 192);
        let orig_handle = pkt.handle();
        pkt.append(b"packet-borrow-payload-0123456789").unwrap();
        let hdr = ShortHeader {
            flags: 0x40,
            dcid: cid,
            packet_number: 300,
            packet_number_len: 2,
        };
        let mut hb = [0u8; 16];
        let hl = hdr.encode(&mut hb).unwrap();
        pkt.prepend(&hb[..hl]).unwrap();
        seal_short_packet_in_place_with_pn(&mut pkt, &pkey, 300, cid).unwrap();
        let sealed_handle = pkt.handle();
        assert_ne!(sealed_handle, orig_handle, "{cipher:?} seal should borrow a new handle");
        assert_eq!(pkt.range().start, 192 - hl);
        assert_eq!(pkt.bytes()[0] & FLAG_PROTECTED, FLAG_PROTECTED);

        let pref = ShortHeader::decode_prefix(pkt.bytes()).unwrap();
        open_short_packet_in_place(
            &mut pkt,
            &pkey,
            pref.flags,
            pref.header_len,
            300,
            pref.packet_number_len,
            cid,
        )
        .unwrap();
        assert_ne!(pkt.handle(), sealed_handle, "{cipher:?} open should borrow a new handle");
        assert_eq!(&pkt.bytes()[pref.header_len..], b"packet-borrow-payload-0123456789");
    }

    // Also test CBOR config method 3 tag 6 (`cipher`: 0=GCM, 1=CCM).
    let mut st = State::default();
    assert_eq!(call_host(&mut st, &host, HOST_START), 0);
    let mut cbuf = [0u8; 64];
    let mut w = cbor::Writer::new(&mut cbuf);
    w.map(2).uint(2).bytes(&[0x77; 32]).uint(6).uint(1);
    let req = w.finish().unwrap();
    let cfg_call = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: COMPONENT,
        method: 3,
        flags: 1,
        data: req.as_ptr(),
        len: req.len(),
        ..unsafe { core::mem::zeroed() }
    };
    dispatch(&host, &mut st, &cfg_call);
    let reply = fake.replies.pop().unwrap();
    assert_eq!(
        cbor::get_int(&reply, 5).ok().flatten(),
        Some(1),
        "config tag 5 must report cipher=1 (CCM)"
    );
    assert_eq!(call_host(&mut st, &host, HOST_STOP), 0);
}

#[test]
#[ignore]
fn bench_mod_quic2_crypto_and_abi() {
    use quic_base::{DirectionKeyMaterial, PacketCrypto, ProviderAeadKey};
    use quic_link::{derive_device_secret_with, derive_peer_api_key_with};
    use std::hint::black_box;
    use std::time::Instant;

    let mut fake = FakeHost::default();
    let host = make_host(&mut fake);
    let hw = unsafe { Hw::new(&host) }.unwrap();
    let crypto_client = CryptoClient::from_host(hw).unwrap();
    let psa_provider = psa_crypto_provider(hw);

    let key = [0x42u8; 16];
    let mut nonce = [0x24u8; 12];
    let aad = [0x01u8; 11];
    let pkey = ProviderAeadKey::new(psa_provider, DirectionKeyMaterial::from_material(&[0x42u8; 28]));

    std::println!("=== mod_quic2 CryptoClient ABI + FakeHost PSA benchmark (host release) ===");
    std::println!(
        "{:<8} {:<30} {:>10} {:>10} {:>10}",
        "Size", "Layer", "Wall ns/op", "CPU ns/op", "MiB/s"
    );
    for &sz in &[64usize, 256, 1024, 1200, 1400] {
        let iters = match sz {
            0..=256 => 40_000usize,
            257..=1200 => 20_000,
            _ => 12_000,
        };
        let mut buf = vec![0x5au8; sz];
        for _ in 0..200 {
            let tag = crypto_client.aes_gcm_seal(&key, &nonce, &aad, &mut buf).unwrap();
            crypto_client
                .aes_gcm_open(&key, &nonce, &aad, &mut buf, &tag)
                .unwrap();
        }

        let c0 = bench_thread_cpu_ns();
        let t0 = Instant::now();
        for i in 0..iters {
            nonce[8..12].copy_from_slice(&(i as u32).to_be_bytes());
            let tag = crypto_client
                .aes_gcm_seal(&key, &nonce, &aad, black_box(&mut buf))
                .unwrap();
            black_box(tag);
        }
        let seal_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let seal_c = bench_thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        nonce[8..12].copy_from_slice(&0u32.to_be_bytes());
        let mut cipher = vec![0x5au8; sz];
        let fixed_tag = crypto_client
            .aes_gcm_seal(&key, &nonce, &aad, &mut cipher)
            .unwrap();
        let mut scratch = vec![0u8; sz];
        let c0 = bench_thread_cpu_ns();
        let t0 = Instant::now();
        for _ in 0..iters {
            scratch.copy_from_slice(&cipher);
            crypto_client
                .aes_gcm_open(&key, &nonce, &aad, black_box(&mut scratch), &fixed_tag)
                .unwrap();
        }
        let open_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let open_c = bench_thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        let c0 = bench_thread_cpu_ns();
        let t0 = Instant::now();
        for i in 0..iters {
            let tag = pkey.seal_in_place(i as u32, &aad, black_box(&mut buf)).unwrap();
            pkey.open_in_place(i as u32, &aad, black_box(&mut buf), &tag)
                .unwrap();
        }
        let p_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let p_c = bench_thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        let c0 = bench_thread_cpu_ns();
        let t0 = Instant::now();
        for i in 0..iters {
            nonce[8..12].copy_from_slice(&(i as u32).to_be_bytes());
            let tag = crypto_client
                .aead_seal(ALG_CCM, &key, &nonce, &aad, black_box(&mut buf))
                .unwrap();
            crypto_client
                .aead_open(ALG_CCM, &key, &nonce, &aad, black_box(&mut buf), &tag)
                .unwrap();
        }
        let ccm_w = t0.elapsed().as_nanos() as f64 / iters as f64;
        let ccm_c = bench_thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64;

        let pool_b = XipPool::new(hw);
        let pkey_ccm = ProviderAeadKey::new(
            psa_crypto_provider_for(hw, AeadCipher::Ccm),
            DirectionKeyMaterial::from_material(&[0x42u8; 28]),
        );
        let cid_b = ConnectionId::new(1).unwrap();
        let bench_pkt_borrow = |pk: &ProviderAeadKey| -> (f64, f64) {
            let c0 = bench_thread_cpu_ns();
            let t0 = Instant::now();
            for i in 0..iters {
                let mut pkt = quic_mux::PacketPool::alloc(&pool_b).unwrap();
                pkt.append(&buf).unwrap();
                let hdr = ShortHeader {
                    flags: 0x40,
                    dcid: cid_b,
                    packet_number: i as u32,
                    packet_number_len: 2,
                };
                let mut hb = [0u8; 16];
                let hl = hdr.encode(&mut hb).unwrap();
                pkt.prepend(&hb[..hl]).unwrap();
                quic_base::seal_short_packet_in_place_with_pn(&mut pkt, pk, i as u32, cid_b)
                    .unwrap();
                let pref = ShortHeader::decode_prefix(pkt.bytes()).unwrap();
                quic_base::open_short_packet_in_place(
                    &mut pkt,
                    pk,
                    pref.flags,
                    pref.header_len,
                    i as u32,
                    pref.packet_number_len,
                    cid_b,
                )
                .unwrap();
                black_box(pkt.bytes().len());
            }
            (
                t0.elapsed().as_nanos() as f64 / iters as f64,
                bench_thread_cpu_ns().saturating_sub(c0) as f64 / iters as f64,
            )
        };
        let (pb_gcm_w, pb_gcm_c) = bench_pkt_borrow(&pkey);
        let (pb_ccm_w, pb_ccm_c) = bench_pkt_borrow(&pkey_ccm);

        for (lbl, w, c) in [
            ("CryptoClient::aes_gcm_seal", seal_w, seal_c),
            ("CryptoClient::aes_gcm_open", open_w, open_c),
            ("CryptoClient GCM seal+open", seal_w + open_w, seal_c + open_c),
            ("ProviderAeadKey(GCM) in-pl", p_w, p_c),
            ("CryptoClient CCM seal+open", ccm_w, ccm_c),
            ("PktBorrow(GCM) seal+open", pb_gcm_w, pb_gcm_c),
            ("PktBorrow(CCM) seal+open", pb_ccm_w, pb_ccm_c),
        ] {
            let mibs = (sz as f64 * 1e9) / (w * 1024.0 * 1024.0);
            std::println!("{:<8} {:<30} {:>10.1} {:>10.1} {:>10.1}", sz, lbl, w, c, mibs);
        }
    }
    assert!(fake.held.is_empty(), "leaked scratch packets: {:?}", fake.held.len());

    // Part B: End-to-end `mod_quic2` (`FakeHost`) UDP/PPP stream upload + reply across Crypto OFF vs ON.
    std::println!("\n=== mod_quic2 FakeHost UDP/PPP Stream Benchmark (Crypto OFF vs ON) ===");
    std::println!(
        "{:<8} {:<22} {:>10} {:>10} {:>9} {:>9} {:>7} {:>9}",
        "Bytes", "Mode", "Wall us", "CPU us", "MiB/s", "Mbps", "TX pkts", "Wire B"
    );

    let esp_pub = [0x03u8; PUBLIC_KEY_COMPRESSED_LEN];
    let owner_pub = [0x02u8; PUBLIC_KEY_COMPRESSED_LEN];
    let node_secret = derive_device_secret_with(
        &psa_provider,
        b"control-plane-root-secret-material",
        &esp_pub,
    )
    .unwrap();
    let prekey_api_key = derive_peer_api_key_with(&psa_provider, &node_secret, &[]).unwrap();
    let init_api_key = derive_peer_api_key_with(&psa_provider, &node_secret, &owner_pub).unwrap();

    for &(sz, iters) in &[
        (64usize, 50usize),
        (256, 50),
        (1024, 50),
        (1400, 50),
        (64 * 1024, 10),
        (256 * 1024, 5),
        (512 * 1024, 3),
    ] {
        let payload: Vec<u8> = (0..sz).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
        for &(label, link_is_serial, cipher, encrypt, use_initial) in &[
            ("PPP(1472) crypto=OFF", true, AeadCipher::Gcm, false, false),
            ("PPP(1472) GCM(pk)", true, AeadCipher::Gcm, true, false),
            ("PPP(1472) CCM(pk)", true, AeadCipher::Ccm, true, false),
            ("UDP(1472) crypto=OFF", false, AeadCipher::Gcm, false, false),
            ("UDP(1472) GCM(pk)", false, AeadCipher::Gcm, true, false),
            ("UDP(1472) CCM(pk)", false, AeadCipher::Ccm, true, false),
            ("UDP(1472) GCM(init)", false, AeadCipher::Gcm, true, true),
            ("UDP(1472) CCM(init)", false, AeadCipher::Ccm, true, true),
        ] {
            let mut sum_w = 0.0;
            let mut sum_c = 0.0;
            let mut last_tx = 0usize;
            let mut last_wire = 0usize;
            for iter in 0..iters {
                let mut f2 = FakeHost::default();
                let h2 = make_host(&mut f2);
                let hw2 = unsafe { Hw::new(&h2) }.unwrap();
                let prov2 = psa_crypto_provider_for(hw2, cipher);
                let mut st2 = State::default();
                assert_eq!(call_host(&mut st2, &h2, HOST_START), 0);
                st2.set_cipher(cipher);
                if encrypt {
                    st2.configure_crypto(Some(esp_pub), Some(node_secret), None);
                }
                st2.set_udp_encryption(!link_is_serial && encrypt);
                st2.set_serial_encryption(link_is_serial && encrypt);

                let pool: StaticPacketPool<32> = StaticPacketPool::new();
                let mut mux = QuicMux::new(&pool, embedded_mux_limits());
                mux.add_association(OWNER_ASSOC, true).unwrap();
                let mut cfg = if use_initial {
                    let mut c = client_handshake_config(22);
                    c.local_params.public_key = Some(owner_pub);
                    c
                } else {
                    default_assoc_config()
                };
                cfg.ack_eliciting_threshold = 4;
                let mut assoc: QuicLinkAssoc<StaticPacketLease<'_, 32>> = QuicLinkAssoc::new(cfg);
                if link_is_serial {
                    assoc.upsert_link(
                        LINK_SERIAL,
                        serial_link_profile().with_encryption(encrypt),
                    );
                } else {
                    assoc.upsert_link(LINK_UDP, udp_link_profile().with_encryption(encrypt));
                }
                if encrypt {
                    assoc
                        .set_crypto_config(AssocCryptoConfig {
                            provider: prov2,
                            role: CryptoRole::Client,
                            local_public_key: use_initial.then_some(owner_pub),
                            local_random: [0x22; 32],
                            send_api_key: Some(if use_initial {
                                init_api_key
                            } else {
                                prekey_api_key
                            }),
                            node_secret: None,
                        })
                        .unwrap();
                }

                let link_id = if link_is_serial { LINK_SERIAL } else { LINK_UDP };
                let (from, token) = if link_is_serial {
                    (ADDR_LINK + 1, QUIC2_SERIAL_PORT)
                } else {
                    (ADDR_LINK + 2, 0x777u64)
                };
                let tick_call = Call {
                    size: core::mem::size_of::<Call>() as u32,
                    component: COMPONENT,
                    method: TICK,
                    ..unsafe { core::mem::zeroed() }
                };

                let stream_id = mux.open_stream(OWNER_ASSOC, true).unwrap();
                let mut sent = 0usize;
                let mut fin_sent = false;
                let mut rx_cb = ClientRx::default();
                let mut tx_pkts = 0usize;
                let mut tx_wire = 0usize;
                let mut replied = false;

                let c0 = bench_thread_cpu_ns();
                let t0 = Instant::now();
                if use_initial {
                    let mut pr = &pool;
                    assoc.enqueue_initial(&mut pr).unwrap();
                }
                for _ in 0..200_000 {
                    f2.now_us += 200;
                    if !use_initial || assoc.is_handshake_complete() {
                        while !fin_sent {
                            let Ok(cap) = mux.stream_send_capacity(OWNER_ASSOC, stream_id, &assoc)
                            else {
                                break;
                            };
                            if !cap.can_send_now || cap.max_chunk_bytes == 0 {
                                break;
                            }
                            let n = (sz - sent).min(cap.max_chunk_bytes);
                            let fin = sent + n == sz;
                            match mux.write_stream(
                                OWNER_ASSOC,
                                stream_id,
                                TxStreamData::Bytes(&payload[sent..sent + n]),
                                fin,
                                &mut assoc,
                            ) {
                                Ok(w) => {
                                    sent += w;
                                    if fin && w == n {
                                        fin_sent = true;
                                    } else if w == 0 {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    let _ = mux.flush_control(OWNER_ASSOC, &mut assoc);
                    let mut moved = false;
                    while let Some(out) = assoc.pop_outbound_packet() {
                        moved = true;
                        tx_pkts += 1;
                        tx_wire += out.packet.bytes().len();
                        assert_eq!(
                            call_link(
                                &mut st2,
                                &h2,
                                &mut f2,
                                if link_is_serial { 0 } else { RX },
                                from,
                                token,
                                out.packet.bytes()
                            ),
                            0
                        );
                    }
                    if !replied
                        && f2
                            .consumers
                            .get(&1)
                            .is_some_and(|c| c.fin && c.received.len() == sz)
                    {
                        replied = true;
                        f2.queue_consumer_reply(1, b"ok", true);
                    }
                    dispatch(&h2, &mut st2, &tick_call);
                    for (_lk, _peer, wire) in core::mem::take(&mut f2.link_tx) {
                        moved = true;
                        let mut p = (&pool).alloc().unwrap();
                        p.append(&wire).unwrap();
                        let demux = assoc.receive_packet(link_id, p, f2.now_us).unwrap();
                        if let Some(mp) = demux.mux_packet {
                            mux.receive_packet(
                                RxMeta {
                                    association: OWNER_ASSOC,
                                    received_at_us: f2.now_us,
                                },
                                mp,
                                &mut assoc,
                                &mut rx_cb,
                            )
                            .unwrap();
                        }
                    }
                    if rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin) {
                        break;
                    }
                    if !moved {
                        f2.now_us += 25_000;
                        let mut pr = &pool;
                        let _ = assoc.poll_timers(&mut pr, f2.now_us);
                        let _ = mux.advance_time(f2.now_us, &mut assoc, &mut rx_cb);
                        dispatch(&h2, &mut st2, &tick_call);
                    }
                }
                sum_w += t0.elapsed().as_nanos() as f64;
                sum_c += bench_thread_cpu_ns().saturating_sub(c0) as f64;
                last_tx = tx_pkts;
                last_wire = tx_wire;
                if iter == 0 {
                    assert_eq!(f2.consumers[&1].received, payload);
                    assert_eq!(rx_cb.streams[&stream_id].0, b"ok");
                }
                assert_eq!(call_host(&mut st2, &h2, HOST_STOP), 0);
            }
            let wall_us = (sum_w / iters as f64) / 1_000.0;
            let cpu_us = (sum_c / iters as f64) / 1_000.0;
            let mibs = (sz as f64 * 1e6) / (wall_us * 1024.0 * 1024.0);
            let mbps = (sz as f64 * 8.0) / wall_us;
            std::println!(
                "{:<8} {:<22} {:>10.1} {:>10.1} {:>9.2} {:>9.2} {:>7} {:>9}",
                sz,
                label,
                wall_us,
                cpu_us,
                mibs,
                mbps,
                last_tx,
                last_wire
            );
        }
    }
}
