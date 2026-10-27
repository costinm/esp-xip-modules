//! Optional discovery policy. NAN owns radio timing; this module supplies bytes.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
mod announce;
mod relay;
use xip_abi::{
    cbor::{self, Item, Reader, Writer},
    *,
};
const COMPONENT: i32 = 47;
const NAN: i32 = 46;
const SET_ANNOUNCE: i32 = 11;
const TIMEOUT: i32 = 100;
pub const STATE_BYTES: usize = 8;

#[derive(Default)]
pub struct State {
    context: usize,
}
/// Where the answer to a request goes. The runtime wraps it when the request arrived as an envelope (`CALL_ENVELOPE`): it keeps the envelope's id
/// and matches the answer by (`from`, `token`, component, `method`), so the module may answer later, from another call.
#[derive(Clone, Copy)]
struct Reply {
    from: u32,
    token: u64,
    method: i32,
    wanted: bool,
}
impl Reply {
    fn send(self, hw: &Hw, result: &[u8]) {
        if self.wanted {
            hw.message(self.from, COMPONENT, self.method, self.token, result);
        }
    }
    /// An error is a bare negative integer (`7: error` in an envelope).
    fn error(self, hw: &Hw, rc: i32) {
        let mut b = [0; 16];
        let mut w = Writer::new(&mut b);
        w.int(rc as i64);
        if let Some(b) = w.finish() {
            self.send(hw, b);
        }
    }
}
struct Pending {
    reply: Reply,
    token: u64,
    alarm: i32,
}
struct Context {
    record: [u8; announce::INFO_MAX],
    len: usize,
    channel: u8,
    next_id: u64,
    pending: Option<Pending>,
    relay_links: [u32; 12],
    relay_alarm: u32,
}
impl Context {
    fn refresh(&mut self, hw: &Hw) -> Result<(), i32> {
        let mut record = [0; announce::INFO_MAX];
        let (_, n) = announce::make_announce(hw, self.channel, &mut record)?;
        // Keep the old cache on signing/allocation failure.
        self.record = record;
        self.len = n;
        Ok(())
    }
    fn publish(&mut self, hw: &Hw, reply: Reply) -> Result<(), i32> {
        if self.pending.is_some() {
            return Err(ERR_BUSY);
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let token = self.next_id;
        let mut data = [0; 272];
        let mut w = Writer::new(&mut data);
        w.map(1).uint(1).bytes(&self.record[..self.len]);
        let data = w.finish().ok_or(ERR_SIZE)?;
        let alarm = hw.alarm_set(2000, 0, 0, ALARM_EXACT, COMPONENT, TIMEOUT);
        if alarm < 0 {
            return Err(alarm);
        }
        let rc = hw.message(NAN as u32, NAN, SET_ANNOUNCE, token, data);
        if rc < 0 {
            hw.alarm_cancel(alarm);
            return Err(rc);
        }
        self.pending = Some(Pending {
            reply,
            token,
            alarm,
        });
        Ok(())
    }
    fn fail(&mut self, hw: &Hw, error: i32) {
        if let Some(p) = self.pending.take() {
            hw.alarm_cancel(p.alarm);
            p.reply.error(hw, error);
        }
    }
    fn complete(&mut self, hw: &Hw, result: &[u8]) {
        if let Some(p) = self.pending.take() {
            hw.alarm_cancel(p.alarm);
            p.reply.send(hw, result);
        }
    }
}
fn refresh_field(fields: &[u8]) -> Result<bool, i32> {
    match cbor::get(fields, 1).map_err(|_| ERR_ARG)? {
        None => Ok(false),
        Some(mut r) => match r.next() {
            Some(Item::Bool(b)) => Ok(b),
            _ => Err(ERR_ARG),
        },
    }
}
impl State {
    fn start(&mut self, hw: &Hw, data: &[u8]) -> Result<(), i32> {
        if self.context != 0 {
            return Err(ERR_BUSY);
        }
        let channel = if data.is_empty() {
            6
        } else {
            cbor::get_int(data, 1).map_err(|_| ERR_ARG)?.unwrap_or(6)
        };
        if !(1..=13).contains(&channel) {
            return Err(ERR_ARG);
        }
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Context>())
            .cast::<Context>();
        if ptr.is_null() {
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Context {
                record: [0; announce::INFO_MAX],
                len: 0,
                channel: channel as u8,
                next_id: hw.now_us().unwrap_or(1) << 8,
                pending: None,
                relay_links: [0;12], relay_alarm:0,
            });
        }
        if let Err(rc) = unsafe { &mut *ptr }.refresh(hw) {
            unsafe {
                ptr.drop_in_place();
            }
            hw.mem_free(ptr.cast());
            return Err(rc);
        }
        self.context = ptr as usize;
        Ok(())
    }
    fn stop(&mut self, hw: &Hw) {
        if self.context == 0 {
            return;
        }
        let ptr = self.context as *mut Context;
        self.context = 0;
        let cx = unsafe { &mut *ptr };
        relay::stop(hw,cx);
        if let Some(p) = cx.pending.take() {
            hw.alarm_cancel(p.alarm);
            p.reply.error(hw, ERR_NOTFOUND);
        }
        unsafe {
            ptr.drop_in_place();
        }
        hw.mem_free(ptr.cast());
    }
    /// The runtime's multicast link delivered a tagged discovery request
    /// `{1:6, 2:2, 3:id, 5:{}}`. Reply with the cached signed announce,
    /// correlated to the request's id, to the source token it arrived with.
    /// The signed bytes are reused; only the correlation id and the (cheap)
    /// outer record change, so a sweep costs no re-signing.
    fn discovery_reply(hw: &Hw, cx: &Context, call: &CallView) {
        use dmesh_server::announce as a;
        let record = match dmesh_server::tagged::decode(call.data) {
            Some(r)
                if r.component == Some(dmesh_server::tagged::Name::Tag(6))
                    && r.method == Some(dmesh_server::tagged::Name::Tag(2)) =>
            {
                r
            }
            _ => return,
        };
        let id = match record.id {
            Some(id) => id,
            None => return,
        };
        let own = match a::decode_announce(&cx.record[..cx.len]) {
            Some(a) => a,
            None => return,
        };
        let mut out = [0u8; 384];
        let n = match a::encode_discovery_response(own, id, &mut out) {
            Some(n) => n,
            None => return,
        };
        // Not a link: the datagram's source rides on the call; the module sends
        // from the runtime UDP socket it received on.
        let Some((ip, port, scope)) = call.src else { return };
        let rc = hw.udp_send(call.from, (ip, port, scope), &out[..n]);
        debug_assert!(rc != ERR_UNSUPPORTED, "the host that delivered a UDP datagram has a UDP send");
    }
}

// TODO: switch to vtable
impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_START) {
            return self
                .start(hw, call.data)
                .map_or_else(|rc| rc, |_| RET_CONTINUE as i32);
        }
        if call.is_host(HOST_STOP) {
            self.stop(hw);
            return RET_CONTINUE as i32;
        }
        if self.context == 0 {
            return RET_CONTINUE as i32;
        }
        let cx = unsafe { &mut *(self.context as *mut Context) };
        // A raw datagram on the discovery port: the runtime's multicast link
        // delivered a tagged discovery request. Reply with the cached signed
        // record, correlated to the request's id, to the source it came from.
        if call.flags & CALL_LINK != 0 && !call.is_envelope() {
            Self::discovery_reply(hw, cx, call);
            return RET_CONTINUE as i32;
        }
        if call.from == NAN as u32 && call.component == NAN && call.method == SET_ANNOUNCE {
            if cx.pending.as_ref().is_some_and(|p| p.token == call.id) {
                let mut r = Reader::new(call.data);
                let v = r.next();
                if r.rest().is_empty() && matches!(v, Some(Item::Map(0) | Item::Neg(_))) {
                    cx.complete(hw, call.data);
                } else {
                    cx.fail(hw, ERR_IO);
                }
            }
            return RET_CONTINUE as i32;
        }
        if call.component == COMPONENT && call.method == TIMEOUT && call.flags & CALL_LINK == 0 {
            if let Some(p) = cx.pending.take() {
                p.reply.error(hw, ERR_BUSY);
            }
            return RET_CONTINUE as i32;
        }
        if call.component==COMPONENT && call.method==relay::TICK && !call.is_envelope(){relay::tick(hw,cx);return RET_CONTINUE as i32;}
        // Unrelated/stale module replies are data, not calls to our API.
        // In particular, never bounce an unsupported response back forever.
        // A raw frame of the module's port is not a request either: the runtime parses envelopes and delivers them as requests.
        if (call.flags & CALL_LINK != 0 && !call.is_envelope()) || call.component != COMPONENT {
            return RET_CONTINUE as i32;
        }
        let (method, fields) = (call.method, call.data);
        let reply = Reply {
            from: call.from,
            token: call.id,
            method,
            wanted: call.wants_response(),
        };
        let envelope = call.is_envelope();
        let result = (|| {
            if matches!(method, 3..=9) { return relay::handle(hw, cx, method, fields, reply); }
            if !matches!(method, 1 | 2) {
                return Err(ERR_UNSUPPORTED);
            }
            // A synchronous application request ends when this handler returns;
            // it cannot retain a waiter for another module's asynchronous ACK.
            if method == 2 && !envelope && reply.from == ADDR_MAIN {
                return Err(ERR_UNSUPPORTED);
            }
            if method == 2 && cx.pending.is_some() {
                return Err(ERR_BUSY);
            }
            if refresh_field(fields)? {
                cx.refresh(hw)?;
            }
            if method == 2 {
                cx.publish(hw, reply)?;
                return Ok(());
            }
            let mut out = [0; 272];
            let mut w = Writer::new(&mut out);
            w.map(1).uint(1).bytes(&cx.record[..cx.len]);
            reply.send(hw, w.finish().ok_or(ERR_SIZE)?);
            Ok(())
        })();
        if let Err(rc) = result {
            reply.error(hw, rc);
        }
        RET_CONTINUE as i32
    }
}
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, vec::Vec};
    struct Sent {
        dst: u32,
        component: i32,
        method: i32,
        id: u64,
        data: Vec<u8>,
    }
    #[derive(Default)]
    struct Fake {
        next: u32,
        packets: BTreeMap<u32, Vec<u8>>,
        sent: Vec<Sent>,
        udp: Vec<(u32, [u8; 16], u16, u32, Vec<u8>)>,
        alarms: i32,
        canceled: Vec<i32>,
        frees: usize,
        fail_send: bool,
    }
    unsafe fn fake<'a>(p: *mut core::ffi::c_void) -> &'a mut Fake {
        &mut *p.cast()
    }
    unsafe extern "C" fn alloc(p: *mut core::ffi::c_void, out: *mut u32) -> i32 {
        let f = fake(p);
        f.next += 1;
        *out = f.next;
        f.packets.insert(f.next, std::vec![0;1500]);
        0
    }
    unsafe extern "C" fn buf(
        p: *mut core::ffi::c_void,
        id: u32,
        out: *mut *mut u8,
        cap: *mut usize,
    ) -> i32 {
        let bytes = fake(p).packets.get_mut(&id).unwrap();
        *out = bytes.as_mut_ptr();
        *cap = bytes.len();
        0
    }
    unsafe extern "C" fn free(p: *mut core::ffi::c_void, id: u32) -> i32 {
        assert!(fake(p).packets.remove(&id).is_some());
        0
    }
    unsafe extern "C" fn send(
        p: *mut core::ffi::c_void,
        id: u32,
        n: usize,
        dst: u32,
        c: i32,
        m: i32,
        token: u64,
    ) -> i32 {
        let f = fake(p);
        let mut data = f.packets.remove(&id).expect("send consumes packet");
        data.truncate(n);
        if f.fail_send {
            return ERR_NOTFOUND;
        }
        f.sent.push(Sent {
            dst,
            component: c,
            method: m,
            id: token,
            data,
        });
        0
    }
    unsafe extern "C" fn udp_send(
        p: *mut core::ffi::c_void,
        handle: u32,
        ip: *const u8,
        port: u16,
        scope: u32,
        data: *const u8,
        len: usize,
    ) -> i32 {
        if ip.is_null() || (data.is_null() && len != 0) {
            return ERR_ARG;
        }
        let f = fake(p);
        let bytes = std::slice::from_raw_parts(data, len).to_vec();
        let mut ip0 = [0u8; 16];
        ip0.copy_from_slice(std::slice::from_raw_parts(ip, 16));
        f.udp.push((handle, ip0, port, scope, bytes));
        0
    }
    unsafe extern "C" fn alarm(
        p: *mut core::ffi::c_void,
        delay: u32,
        _: u32,
        _: u32,
        _: u32,
        c: i32,
        m: i32,
    ) -> i32 {
        assert_eq!((delay, c, m), (2000, COMPONENT, TIMEOUT));
        let f = fake(p);
        f.alarms += 1;
        f.alarms
    }
    unsafe extern "C" fn cancel(p: *mut core::ffi::c_void, id: i32) -> i32 {
        fake(p).canceled.push(id);
        0
    }
    unsafe extern "C" fn release(p: *mut core::ffi::c_void, ptr: *mut core::ffi::c_void) -> i32 {
        fake(p).frees += 1;
        std::alloc::dealloc(ptr.cast(), core::alloc::Layout::new::<Context>());
        0
    }
    fn host(f: &mut Fake) -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION;
        h.size = core::mem::size_of::<Host>() as u32;
        h.user = (f as *mut Fake).cast();
        h.pkt_alloc = Some(alloc);
        h.pkt_buf = Some(buf);
        h.pkt_free = Some(free);
        h.send = Some(send);
        h.udp_send = Some(udp_send);
        h.alarm_set = Some(alarm);
        h.alarm_cancel = Some(cancel);
        h.mem_free = Some(release);
        h
    }
    fn state() -> State {
        let mut cx = Context {
            record: [0; announce::INFO_MAX],
            len: 6,
            channel: 6,
            next_id: 100,
            pending: None,
            relay_links: [0;12], relay_alarm:0,
        };
        cx.record[..6].copy_from_slice(b"cached");
        State {
            context: std::boxed::Box::into_raw(std::boxed::Box::new(cx)) as usize,
        }
    }
    fn call<'a>(method: i32, data: &'a [u8]) -> CallView<'a> {
        CallView {
            component: COMPONENT,
            method,
            value: 0,
            flags: 1,
            data,
            time_us: 0,
            pkt: 0,
            from: 52,
            id: 19, request_id: None, src: None }
    }
    #[test]
    fn repeated_reads_do_not_need_crypto_and_failed_refresh_keeps_the_cache() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut s = state();
        for _ in 0..2 {
            s.handle(&hw, &call(1, &[0xa0]));
        }
        assert_eq!(f.sent[0].data, f.sent[1].data);
        assert_eq!(
            cbor::get(&f.sent[0].data, 1).unwrap().unwrap().next(),
            Some(Item::Bytes(b"cached"))
        );
        s.handle(&hw, &call(1, &[0xa1, 1, 0xf5]));
        assert_eq!(
            Reader::new(&f.sent.last().unwrap().data)
                .next()
                .unwrap()
                .as_int(),
            Some(ERR_UNSUPPORTED as i64)
        );
        s.handle(&hw, &call(1, &[0xa0]));
        assert_eq!(f.sent[0].data, f.sent.last().unwrap().data);
        s.stop(&hw);
        assert_eq!(f.frees, 1);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn publish_completes_only_after_the_exact_nan_public_method_reply() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut s = state();
        // an envelope {1: 47, 2: 2, 3: id, 5: {}} as the runtime hands it over: component, method and the fields, answered by (from, token)
        let mut request = call(2, &[0xa0]);
        request.flags = 1 | CALL_ENVELOPE | CALL_LINK;
        request.from = ADDR_LINK;
        request.id = 9;
        s.handle(&hw, &request);
        assert_eq!(f.sent.len(), 1);
        let sent = &f.sent[0];
        assert_eq!((sent.dst, sent.component, sent.method), (46, 46, 11));
        assert_eq!(
            cbor::get(&sent.data, 1).unwrap().unwrap().next(),
            Some(Item::Bytes(b"cached"))
        );
        let token = sent.id;
        let mut ack = call(11, &[0xa0]);
        ack.component = 46;
        ack.from = 46;
        ack.id = token + 1;
        s.handle(&hw, &ack);
        assert_eq!(f.sent.len(), 1);
        ack.id = token;
        ack.from = 20;
        s.handle(&hw, &ack);
        assert_eq!(f.sent.len(), 1);
        ack.from = 46;
        s.handle(&hw, &ack);
        assert_eq!(f.sent.len(), 2);
        let reply = &f.sent[1];
        // the runtime matches this answer to the envelope by (link, token, component, method) and adds {1, 2, 3, 6: ..}
        assert_eq!((reply.dst, reply.id, reply.component, reply.method), (ADDR_LINK, 9, COMPONENT, 2));
        assert_eq!(reply.data, [0xa0]);
        s.handle(&hw, &ack);
        assert_eq!(f.sent.len(), 2, "duplicate ACK is not another request");
        assert_eq!(f.canceled, [1]);
        s.stop(&hw);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn pending_publication_is_bounded_and_stop_or_timeout_releases_it() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut s = state();
        s.handle(&hw, &call(2, &[0xa0]));
        s.handle(&hw, &call(2, &[0xa0]));
        assert_eq!(
            Reader::new(&f.sent.last().unwrap().data)
                .next()
                .unwrap()
                .as_int(),
            Some(ERR_BUSY as i64)
        );
        assert_eq!(f.alarms, 1);
        let mut timeout = call(TIMEOUT, &[]);
        timeout.flags = 0;
        s.handle(&hw, &timeout);
        assert!(unsafe { &*(s.context as *const Context) }.pending.is_none());
        s.handle(&hw, &call(2, &[0xa0]));
        s.stop(&hw);
        assert_eq!(
            Reader::new(&f.sent.last().unwrap().data)
                .next()
                .unwrap()
                .as_int(),
            Some(ERR_NOTFOUND as i64)
        );
        assert_eq!(f.frees, 1);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn synchronous_application_call_is_rejected_before_queuing_publication() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut s = state();
        let mut request = call(2, &[0xa0]);
        request.from = ADDR_MAIN;
        s.handle(&hw, &request);
        assert_eq!(f.alarms, 0);
        assert_eq!(f.sent.len(), 1);
        assert_eq!(f.sent[0].dst, ADDR_MAIN);
        assert_eq!(
            Reader::new(&f.sent[0].data).next().unwrap().as_int(),
            Some(ERR_UNSUPPORTED as i64)
        );
        assert!(unsafe { &*(s.context as *const Context) }.pending.is_none());
        s.stop(&hw);
        assert!(f.packets.is_empty());
    }

    #[test]
    fn failed_delivery_and_invalid_ack_do_not_leave_a_pending_wait() {
        let mut f = Fake {
            fail_send: true,
            ..Default::default()
        };
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut s = state();
        s.handle(&hw, &call(2, &[0xa0]));
        assert!(unsafe { &*(s.context as *const Context) }.pending.is_none());
        assert!(f.packets.is_empty());
        assert_eq!(f.canceled, [1]);
        f.fail_send = false;
        s.handle(&hw, &call(2, &[0xa0]));
        let token = f.sent.last().unwrap().id;
        let mut ack = call(11, &[0xf6]);
        ack.component = 46;
        ack.from = 46;
        ack.id = token;
        s.handle(&hw, &ack);
        assert_eq!(
            Reader::new(&f.sent.last().unwrap().data)
                .next()
                .unwrap()
                .as_int(),
            Some(ERR_IO as i64)
        );
        assert!(unsafe { &*(s.context as *const Context) }.pending.is_none());
        s.stop(&hw);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn discovery_request_replies_with_the_cached_announce_correlated_to_the_id() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        // A structurally valid cached announce (the signature is a dummy: the reply
        // reuses the signed bytes; it does not re-sign or re-verify here).
        let mut own = dmesh_server::announce::Announce::discovery([7u8; 16], 8, 0);
        assert!(own.set_public_key(&[2u8; 33]));
        assert!(own.set_signature(&[9u8; 64]));
        let mut record = [0u8; announce::INFO_MAX];
        let n = dmesh_server::announce::encode(own, &mut record).unwrap();
        let mut s = state();
        {
            let cx = unsafe { &mut *(s.context as *mut Context) };
            cx.record[..n].copy_from_slice(&record[..n]);
            cx.len = n;
        }
        // A discovery request {1:6, 2:2, 3:id, 5:{}} as a raw datagram of the runtime's
        // discovery UDP port: `from` its socket handle, `src` the sweep's source.
        let mut req = [0u8; 96];
        let req_len = dmesh_server::announce::encode_discovery_request(42, &mut req).unwrap();
        let mut request = call(0, &req[..req_len]);
        request.flags = CALL_LINK;
        request.from = 77; // the discovery port's socket handle
        request.id = 1234; // unused on a UDP bearer: no link, no token
        request.src = Some(([9u8; 16], 3339, 2)); // the host's sweep source
        s.handle(&hw, &request);
        assert_eq!(f.udp.len(), 1, "one reply from the discovery socket to the sweep source");
        let (handle, ip, port, scope, data) = &f.udp[0];
        assert_eq!((*handle, *port, *scope), (77, 3339, 2), "from the handle to the source");
        assert_eq!(*ip, [9u8; 16]);
        let decoded = dmesh_server::tagged::decode(data).unwrap();
        assert_eq!(decoded.id, Some(42), "correlated to the request's id");
        assert_eq!(decoded.component, Some(dmesh_server::tagged::Name::Tag(6)));
        assert_eq!(decoded.method, Some(dmesh_server::tagged::Name::Tag(2)));
        s.stop(&hw);
        assert!(f.packets.is_empty());
    }
}
