//! Bounded experimental on-demand routing policy. No forwarding is installed here.
use super::*;
use super::nan_api as radio;
use crate::api::{Record, REPLY, REQUEST};
const TICK: i32 = 102;
const RETAIN_US: u64 = 180_000_000;
const COOLDOWN_US: u64 = 60_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Path {
    peer: [u8; 6],
    instance: u8,
    now: bool,
    channel: u8,
    rssi: i8,
}
#[derive(Clone, Copy)]
struct Seen {
    record: Record,
    reverse: Option<Path>,
    until: u64,
    replied: bool,
    at: u64,
}
#[derive(Clone, Copy)]
struct Candidate {
    target: [u8; 6],
    path: Path,
    hops: u8,
    until: u64,
    session: u64,
    sequence: u32,
    request_at: u64,
}
enum Operation {
    Enable(bool),
    Discover(Record),
}
struct Wait {
    reply: Reply,
    token: u64,
    until: u64,
    operation: Operation,
}
pub(super) struct Routing {
    local: [u8; 6],
    session: u64,
    sequence: u32,
    enabled: bool,
    seen: [Option<Seen>; 8],
    routes: [Option<Candidate>; 8],
    wait: Option<Wait>,
    alarm: i32,
    cooldown: u64,
    admission_at: u64,
    admitted: u8,
    accepted: u32,
    duplicates: u32,
    dropped: u32,
    last_error: i32,
}
impl Routing {
    fn new(local: [u8; 6], session: u64) -> Self {
        Self {
            local,
            session,
            sequence: 0,
            enabled: false,
            seen: [None; 8],
            routes: [None; 8],
            wait: None,
            alarm: -1,
            cooldown: 0,
            admission_at: 0,
            admitted: 0,
            accepted: 0,
            duplicates: 0,
            dropped: 0,
            last_error: 0,
        }
    }
    fn expire(&mut self, now: u64) {
        for slot in &mut self.seen {
            if slot.is_some_and(|s| now >= s.until) {
                *slot = None;
            }
        }
        for slot in &mut self.routes {
            if slot.is_some_and(|s| now >= s.until) {
                *slot = None;
            }
        }
    }
    fn slot(&self, record: &Record) -> Option<usize> {
        self.seen
            .iter()
            .position(|s| s.is_some_and(|s| s.record.same_request(record)))
    }
    fn admission(&mut self, now: u64) -> bool {
        if now >= self.admission_at {
            self.admission_at = now + COOLDOWN_US;
            self.admitted = 0;
        }
        if self.admitted >= 4 {
            return false;
        }
        self.admitted += 1;
        true
    }
    fn send(&mut self, hw: &Hw, record: Record, path: Option<Path>) -> Result<(), i32> {
        let mut payload = [0; wire::RECORD_MAX];
        let payload = record.encode(&mut payload).ok_or(ERR_ARG)?;
        let mut data = [0; 128];
        let request = radio::Solicitation {
            payload,
            delay_ms: if path.is_none() {
                let salt = self
                    .local
                    .iter()
                    .fold(record.session ^ record.sequence as u64, |n, b| {
                        n.wrapping_mul(16777619) ^ *b as u64
                    });
                (salt % 32) as u8
            } else {
                0
            },
            peer: path.map(|p| radio::Peer {
                mac: p.peer,
                instance: p.instance,
            }),
        };
        let rc = hw.message(
            NAN as u32,
            NAN,
            radio::SOLICIT,
            u64::MAX,
            request.encode(&mut data).ok_or(ERR_SIZE)?,
        );
        if rc < 0 {
            Err(rc)
        } else {
            Ok(())
        }
    }
    fn learn(&mut self, record: Record, path: Path, now: u64, request_at: u64) -> Result<(), i32> {
        let existing = self
            .routes
            .iter()
            .position(|r| r.is_some_and(|r| r.target == record.target));
        let index = existing
            .or_else(|| self.routes.iter().position(Option::is_none))
            .ok_or(ERR_NOMEM)?;
        if self.routes[index].is_none_or(|r| {
            request_at > r.request_at || (request_at == r.request_at && record.hops + 1 < r.hops)
        }) {
            self.routes[index] = Some(Candidate {
                target: record.target,
                path,
                hops: record.hops + 1,
                until: now + RETAIN_US,
                session: record.session,
                sequence: record.sequence,
                request_at,
            });
        }
        Ok(())
    }
    fn receive(&mut self, hw: &Hw, record: Record, path: Path, now: u64) -> Result<(), i32> {
        if !self.enabled || !wire::unicast(path.peer) || path.peer == self.local {
            return Err(ERR_ARG);
        }
        self.expire(now);
        if record.kind == REQUEST {
            // First-path policy freezes the reverse tree for this transaction.
            // Neither a different bearer nor an apparent improvement renews power.
            if self.slot(&record).is_some() || record.origin == self.local {
                self.duplicates += 1;
                return Ok(());
            }
            let index = self
                .seen
                .iter()
                .position(Option::is_none)
                .ok_or(ERR_NOMEM)?;
            if !self.admission(now) {
                return Err(ERR_BUSY);
            }
            let hold = hw
                .power()
                .and_then(|p| p.awake_for)
                .ok_or(ERR_UNSUPPORTED)?;
            let rc = unsafe { hold(hw.ctx(), 4000) };
            if rc < 0 {
                return Err(rc);
            }
            self.seen[index] = Some(Seen {
                record,
                reverse: Some(path),
                until: now + RETAIN_US,
                replied: false,
                at: now,
            });
            self.accepted += 1;
            // Refresh the existing subscription so NAN applies the new hold
            // immediately, including a TTL-boundary node that forwards nothing.
            let mut refresh = [0; 16];
            let mut w = Writer::new(&mut refresh);
            w.map(2)
                .uint(1)
                .boolean(true)
                .uint(2)
                .int(wire::INGRESS as i64);
            let rc = hw.message(NAN as u32, NAN, radio::SUBSCRIBE, 0, w.finish().unwrap());
            if rc < 0 {
                return Err(rc);
            }
            if record.target == self.local {
                self.send(
                    hw,
                    Record {
                        kind: REPLY,
                        hops: 0,
                        ..record
                    },
                    Some(path),
                )?;
            } else if record.hops + 1 < record.limit {
                self.send(
                    hw,
                    Record {
                        hops: record.hops + 1,
                        ..record
                    },
                    None,
                )?;
            }
        } else {
            let index = self.slot(&record).ok_or(ERR_NOTFOUND)?;
            let seen = self.seen[index].unwrap();
            if record.target != seen.record.target || record.limit != seen.record.limit {
                return Err(ERR_ARG);
            }
            if seen.replied {
                self.duplicates += 1;
                return Ok(());
            }
            if seen.reverse.is_some_and(|p| p.peer == path.peer) {
                return Err(ERR_ARG);
            }
            let reverse_distance = if seen.reverse.is_some() {
                seen.record.hops + 1
            } else {
                0
            };
            if record.hops + 1 + reverse_distance > record.limit {
                return Err(ERR_ARG);
            }
            self.learn(record, path, now, seen.at)?;
            self.seen[index].as_mut().unwrap().replied = true;
            if let Some(reverse) = seen.reverse {
                if record.hops + 1 >= record.limit {
                    return Err(ERR_ARG);
                }
                self.send(
                    hw,
                    Record {
                        hops: record.hops + 1,
                        ..record
                    },
                    Some(reverse),
                )?;
            }
        }
        Ok(())
    }
    fn arm(&mut self, hw: &Hw) -> Result<(), i32> {
        if self.alarm < 0 {
            self.alarm = hw.alarm_set(2000, 0, 0, ALARM_EXACT | ALARM_WINDOW, COMPONENT, TICK);
            if self.alarm < 0 {
                return Err(self.alarm);
            }
        }
        Ok(())
    }
    fn status(&self, out: &mut [u8]) -> usize {
        let mut w = Writer::new(out);
        w.map(8)
            .uint(1)
            .boolean(self.enabled)
            .uint(2)
            .bytes(&self.local)
            .uint(3)
            .uint(self.accepted as u64)
            .uint(4)
            .uint(self.duplicates as u64)
            .uint(5)
            .uint(self.dropped as u64)
            .uint(6)
            .int(self.last_error as i64)
            .uint(7)
            .uint(self.routes.iter().flatten().count() as u64)
            .uint(8)
            .uint(self.seen.iter().flatten().count() as u64);
        w.finish().map_or(0, |b| b.len())
    }
}
fn routing<'a>(hw: &Hw, cx: &'a mut Context) -> Result<&'a mut Routing, i32> {
    if cx.routing.is_none() {
        let mut state = WifiState {
            size: core::mem::size_of::<WifiState>() as u32,
            ..Default::default()
        };
        let rc = unsafe {
            hw.wifi().and_then(|w| w.state).ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &mut state)
        };
        if rc < 0 {
            return Err(rc);
        }
        if !wire::unicast(state.sta_mac) {
            return Err(ERR_ARG);
        }
        let crypto = CryptoClient::from_host(*hw).ok_or(ERR_UNSUPPORTED)?;
        let mut random = [0; 8];
        crypto.random(&mut random).map_err(|_| ERR_IO)?;
        let session = u64::from_le_bytes(random).max(1);
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Routing>())
            .cast::<Routing>();
        if ptr.is_null() {
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Routing::new(state.sta_mac, session));
        }
        cx.routing = Some(ptr as usize);
    }
    Ok(unsafe { &mut *(cx.routing.unwrap() as *mut Routing) })
}
pub(super) fn stop(hw: &Hw, cx: &mut Context) {
    if let Some(ptr) = cx.routing.take() {
        let r = unsafe { &mut *(ptr as *mut Routing) };
        if r.alarm >= 0 {
            hw.alarm_cancel(r.alarm);
        }
        if let Some(w) = r.wait.take() {
            w.reply.error(hw, ERR_NOTFOUND);
        }
        hw.message(NAN as u32, NAN, radio::SUBSCRIBE, 0, &[0xa1, 1, 0xf4]);
        unsafe {
            core::ptr::drop_in_place(ptr as *mut Routing);
        }
        hw.mem_free(ptr as *mut u8);
    }
}
pub(super) fn event(hw: &Hw, cx: &mut Context, call: &CallView) -> bool {
    if call.component == COMPONENT && call.method == wire::INGRESS {
        if call.from != NAN as u32 || call.is_envelope() || call.flags & CALL_LINK != 0 {
            return true;
        }
        let Some(ptr) = cx.routing else { return true };
        let r = unsafe { &mut *(ptr as *mut Routing) };
        let result = (|| {
            let received = radio::Received::decode(call.data).ok_or(ERR_ARG)?;
            let Some(record) = Record::decode(received.payload) else {
                return Ok(());
            };
            let now = hw.now_us().ok_or(ERR_UNSUPPORTED)?;
            if received.received_us > now || now - received.received_us > 1_000_000 {
                return Err(ERR_ARG);
            }
            let path = Path {
                peer: received.peer.mac,
                instance: received.peer.instance,
                now: received.now,
                channel: received.channel,
                rssi: received.rssi,
            };
            r.receive(hw, record, path, now)
        })();
        if let Err(rc) = result {
            r.dropped += 1;
            r.last_error = rc;
        }
        return true;
    }
    if call.from == NAN as u32 && call.component == NAN && matches!(call.method, 17 | 18) {
        let Some(ptr) = cx.routing else { return true };
        let r = unsafe { &mut *(ptr as *mut Routing) };
        if call.id == u64::MAX {
            if let Some(rc) = Reader::new(call.data)
                .next()
                .and_then(|i| i.as_int())
                .filter(|n| *n < 0)
            {
                r.last_error = rc as i32;
                r.dropped += 1;
            }
            return true;
        }
        if r.wait.as_ref().is_none_or(|w| {
            w.token != call.id
                || match w.operation {
                    Operation::Enable(_) => call.method != 18,
                    Operation::Discover(_) => call.method != 17,
                }
        }) {
            return true;
        }
        let w = r.wait.take().unwrap();
        if r.alarm >= 0 {
            hw.alarm_cancel(r.alarm);
            r.alarm = -1;
        }
        if call.data != [0xa0] {
            let rc = Reader::new(call.data)
                .next()
                .and_then(|i| i.as_int())
                .filter(|n| *n < 0 && *n >= i32::MIN as i64)
                .map_or(ERR_IO, |n| n as i32);
            if let Operation::Discover(record) = w.operation {
                if let Some(index) = r.slot(&record) {
                    r.seen[index] = None;
                }
            }
            w.reply.error(hw, rc);
            r.last_error = rc;
            return true;
        }
        match w.operation {
            Operation::Enable(on) => {
                r.enabled = on;
                let mut out = [0; 128];
                let n = r.status(&mut out);
                w.reply.send(hw, &out[..n]);
                if !on {
                    stop(hw, cx);
                }
            }
            Operation::Discover(record) => {
                let mut out = [0; 64];
                let mut data = Writer::new(&mut out);
                data.map(3)
                    .uint(1)
                    .uint(record.sequence as u64)
                    .uint(2)
                    .uint(record.session)
                    .uint(3)
                    .uint(discovery_timeout(record.limit) / 1000);
                w.reply.send(hw, data.finish().unwrap());
            }
        }
        return true;
    }
    if call.component == COMPONENT
        && call.method == TICK
        && !call.is_envelope()
        && call.flags & CALL_LINK == 0
    {
        if let Some(ptr) = cx.routing {
            let r = unsafe { &mut *(ptr as *mut Routing) };
            let now = hw.now_us().unwrap_or(0);
            r.expire(now);
            if r.wait.as_ref().is_some_and(|w| now >= w.until) {
                r.wait.take().unwrap().reply.error(hw, ERR_BUSY);
            }
            r.alarm = -1;
        }
        return true;
    }
    false
}
fn discovery_timeout(limit: u8) -> u64 {
    2 * limit as u64 * 9_000_000 + 4_000_000
}
pub(super) fn handle(
    hw: &Hw,
    cx: &mut Context,
    method: i32,
    fields: &[u8],
    reply: Reply,
) -> Result<(), i32> {
    if method == 12 && cbor::get(fields, 1).map_err(|_| ERR_ARG)?.is_none() && cx.routing.is_none()
    {
        let mut out = [0; 128];
        let n = Routing::new([0; 6], 0).status(&mut out);
        reply.send(hw, &out[..n]);
        return Ok(());
    }
    if method == 11 && cx.routing.is_none() {
        reply.send(hw, &[0xa1, 1, 0x80]);
        return Ok(());
    }
    cx.next_id = cx.next_id.wrapping_add(1).max(1);
    let token = cx.next_id;
    let r = routing(hw, cx)?;
    let now = hw.now_us().ok_or(ERR_UNSUPPORTED)?;
    r.expire(now);
    if method == 11 {
        let mut out = [0; 1200];
        let mut w = Writer::new(&mut out);
        w.map(1).uint(1).array(r.routes.iter().flatten().count());
        for route in r.routes.iter().flatten() {
            w.map(9)
                .uint(1)
                .bytes(&route.target)
                .uint(2)
                .bytes(&route.path.peer)
                .uint(3)
                .uint(route.hops as u64)
                .uint(4)
                .uint((route.until - now) / 1000)
                .uint(5)
                .uint(route.path.channel as u64)
                .uint(6)
                .int(route.path.rssi as i64)
                .uint(7)
                .boolean(route.path.now)
                .uint(8)
                .uint(route.sequence as u64)
                .uint(9)
                .uint(route.session);
        }
        reply.send(hw, w.finish().ok_or(ERR_SIZE)?);
        return Ok(());
    }
    if method == 12 {
        let enabled = match cbor::get(fields, 1)
            .map_err(|_| ERR_ARG)?
            .and_then(|mut r| r.next())
        {
            None => {
                let mut out = [0; 128];
                let n = r.status(&mut out);
                reply.send(hw, &out[..n]);
                return Ok(());
            }
            Some(Item::Bool(on)) => on,
            _ => return Err(ERR_ARG),
        };
        if r.wait.is_some() {
            return Err(ERR_BUSY);
        }
        r.arm(hw)?;
        let mut data = [0; 32];
        let mut w = Writer::new(&mut data);
        w.map(2)
            .uint(1)
            .boolean(enabled)
            .uint(2)
            .int(wire::INGRESS as i64);
        let rc = hw.message(
            NAN as u32,
            NAN,
            radio::SUBSCRIBE,
            token,
            w.finish().unwrap(),
        );
        if rc < 0 {
            return Err(rc);
        }
        // Disable local ingress immediately while awaiting NAN teardown.
        if !enabled {
            r.enabled = false;
            r.seen = [None; 8];
            r.routes = [None; 8];
        }
        r.wait = Some(Wait {
            reply,
            token,
            until: now + 2_000_000,
            operation: Operation::Enable(enabled),
        });
        return Ok(());
    }
    if !r.enabled {
        return Err(ERR_NOTFOUND);
    }
    if r.wait.is_some() || now < r.cooldown {
        return Err(ERR_BUSY);
    }
    let target = match cbor::get(fields, 1)
        .map_err(|_| ERR_ARG)?
        .and_then(|mut r| r.next())
    {
        Some(Item::Bytes(b)) => b.try_into().map_err(|_| ERR_ARG)?,
        _ => return Err(ERR_ARG),
    };
    let limit = cbor::get_int(fields, 2).map_err(|_| ERR_ARG)?.unwrap_or(4);
    if !(1..=wire::MAX_HOPS as i64).contains(&limit) {
        return Err(ERR_ARG);
    }
    r.sequence = r.sequence.checked_add(1).ok_or(ERR_BUSY)?;
    let record = Record {
        kind: REQUEST,
        origin: r.local,
        session: r.session,
        sequence: r.sequence,
        target,
        hops: 0,
        limit: limit as u8,
    };
    if !record.valid() {
        return Err(ERR_ARG);
    }
    let slot = r.seen.iter().position(Option::is_none).ok_or(ERR_NOMEM)?;
    r.arm(hw)?;
    // Preserve the origin's receiver across the complete slow request/reply operation.
    let hold = hw
        .power()
        .and_then(|p| p.awake_for)
        .ok_or(ERR_UNSUPPORTED)?;
    let timeout = discovery_timeout(record.limit);
    let rc = unsafe { hold(hw.ctx(), (timeout / 1000) as u32) };
    if rc < 0 {
        return Err(rc);
    }
    let mut payload = [0; 80];
    let payload = record.encode(&mut payload).unwrap();
    let mut data = [0; 96];
    let mut w = Writer::new(&mut data);
    w.map(1).uint(1).bytes(payload);
    let rc = hw.message(NAN as u32, NAN, radio::SOLICIT, token, w.finish().unwrap());
    if rc < 0 {
        return Err(rc);
    }
    r.seen[slot] = Some(Seen {
        record,
        reverse: None,
        until: now + timeout,
        replied: false,
        at: now,
    });
    r.cooldown = now + COOLDOWN_US;
    r.wait = Some(Wait {
        reply,
        token,
        until: now + 2_000_000,
        operation: Operation::Discover(record),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::{c_char, c_void, CStr};
    use std::{collections::BTreeMap, vec::Vec};
    #[derive(Default)]
    struct Fake {
        packets: BTreeMap<u32, Vec<u8>>,
        next: u32,
        sent: Vec<Vec<u8>>,
        holds: Vec<u32>,
        reject: i32,
        messages: Vec<(u32, i32, i32, u64)>,
        released: usize,
    }
    unsafe extern "C" fn allocate(p: *mut c_void, out: *mut u32) -> i32 {
        let f = &mut *(p as *mut Fake);
        f.next += 1;
        *out = f.next;
        f.packets.insert(f.next, std::vec![0;1500]);
        0
    }
    unsafe extern "C" fn buffer(
        p: *mut c_void,
        id: u32,
        out: *mut *mut u8,
        cap: *mut usize,
    ) -> i32 {
        let b = (*(p as *mut Fake)).packets.get_mut(&id).unwrap();
        *out = b.as_mut_ptr();
        *cap = b.len();
        0
    }
    unsafe extern "C" fn send(
        p: *mut c_void,
        id: u32,
        n: usize,
        dst: u32,
        c: i32,
        m: i32,
        token: u64,
    ) -> i32 {
        assert!(dst == 46 || dst == 52);
        let f = &mut *(p as *mut Fake);
        let mut b = f.packets.remove(&id).unwrap();
        b.truncate(n);
        f.sent.push(b);
        f.messages.push((dst, c, m, token));
        0
    }
    unsafe extern "C" fn hold(p: *mut c_void, ms: u32) -> i32 {
        let f = &mut *(p as *mut Fake);
        if f.reject < 0 {
            return f.reject;
        }
        f.holds.push(ms);
        ms as i32
    }
    static POWER: Power = Power {
        version: POWER_VERSION,
        size: core::mem::size_of::<Power>() as u32,
        awake_for: Some(hold),
        ..unsafe { core::mem::zeroed() }
    };
    unsafe extern "C" fn component(_: *mut c_void, name: *const c_char) -> *const c_void {
        if CStr::from_ptr(name) == c"power" {
            (&POWER as *const Power).cast()
        } else {
            core::ptr::null()
        }
    }
    fn host(f: &mut Fake) -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION;
        h.size = core::mem::size_of::<Host>() as u32;
        h.user = (f as *mut Fake).cast();
        h.pkt_alloc = Some(allocate);
        h.pkt_buf = Some(buffer);
        h.send = Some(send);
        h.get_component = Some(component);
        h
    }
    fn node(mac: [u8; 6]) -> Routing {
        let mut r = Routing::new(mac, 17);
        r.enabled = true;
        r
    }
    fn path(mac: [u8; 6]) -> Path {
        Path {
            peer: mac,
            instance: 3,
            now: false,
            channel: 6,
            rssi: -70,
        }
    }
    fn request() -> Record {
        Record {
            kind: REQUEST,
            origin: [2; 6],
            target: [6; 6],
            session: 17,
            sequence: 1,
            hops: 0,
            limit: 4,
        }
    }
    fn output(f: &Fake) -> (Record, Option<radio::Peer>) {
        let p = radio::Solicitation::decode(
            f.sent
                .iter()
                .zip(&f.messages)
                .rev()
                .find(|(_, m)| m.2 == radio::SOLICIT)
                .unwrap()
                .0,
        )
        .unwrap();
        (Record::decode(p.payload).unwrap(), p.peer)
    }
    #[test]
    fn two_hop_request_reply_learns_both_candidates_and_cross_bearer_duplicates_do_not_wake() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let q = request();
        let mut a = node(q.origin);
        let mut b = node([4; 6]);
        let mut c = node(q.target);
        a.seen[0] = Some(Seen {
            record: q,
            reverse: None,
            until: 100_000_000,
            replied: false,
            at: 0,
        });
        b.receive(&hw, q, path(a.local), 1).unwrap();
        let (forward, peer) = output(&f);
        assert!(peer.is_none());
        assert_eq!(forward.hops, 1);
        b.receive(
            &hw,
            q,
            Path {
                now: true,
                ..path(a.local)
            },
            2,
        )
        .unwrap();
        assert_eq!(f.holds, [4000]);
        assert_eq!(
            f.messages.iter().filter(|m| m.2 == radio::SOLICIT).count(),
            1
        );
        c.receive(&hw, forward, path(b.local), 3).unwrap();
        let (reply, peer) = output(&f);
        assert_eq!(reply.kind, REPLY);
        assert_eq!(peer.unwrap().mac, b.local);
        b.receive(&hw, reply, path(c.local), 4).unwrap();
        let (reply, peer) = output(&f);
        assert_eq!(reply.hops, 1);
        assert_eq!(peer.unwrap().mac, a.local);
        a.receive(&hw, reply, path(b.local), 5).unwrap();
        assert_eq!(a.routes[0].unwrap().hops, 2);
        assert_eq!(a.routes[0].unwrap().path.peer, b.local);
        assert_eq!(b.routes[0].unwrap().hops, 1);
        assert_eq!(f.holds, [4000, 4000]);
        let count = f.sent.len();
        b.receive(
            &hw,
            Record { hops: 0, ..reply },
            Path {
                now: true,
                ..path(c.local)
            },
            6,
        )
        .unwrap();
        assert_eq!(f.sent.len(), count);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn hop_limit_and_unmatched_or_wrong_target_replies_cannot_build_routes() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut b = node([4; 6]);
        let q = Record {
            limit: 1,
            ..request()
        };
        b.receive(&hw, q, path(q.origin), 1).unwrap();
        assert!(!f.messages.iter().any(|m| m.2 == radio::SOLICIT));
        let unmatched = Record {
            kind: REPLY,
            sequence: 2,
            ..q
        };
        assert_eq!(
            b.receive(&hw, unmatched, path(q.target), 2),
            Err(ERR_NOTFOUND)
        );
        assert_eq!(
            b.receive(
                &hw,
                Record {
                    kind: REPLY,
                    target: [8; 6],
                    ..q
                },
                path(q.target),
                3
            ),
            Err(ERR_ARG)
        );
        assert!(b.routes.iter().all(Option::is_none));
        let mut target = node(q.target);
        target.receive(&hw, q, path(q.origin), 4).unwrap();
        assert_eq!(output(&f).0.hops, 0);
    }
    #[test]
    fn admission_rate_and_live_table_capacity_do_not_evict_deduplication() {
        let mut f = Fake::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut b = node([4; 6]);
        for sequence in 1..=4 {
            b.receive(
                &hw,
                Record {
                    sequence,
                    ..request()
                },
                path([2; 6]),
                10,
            )
            .unwrap();
        }
        assert_eq!(
            b.receive(
                &hw,
                Record {
                    sequence: 5,
                    ..request()
                },
                path([2; 6]),
                11
            ),
            Err(ERR_BUSY)
        );
        for sequence in 5..=8 {
            b.receive(
                &hw,
                Record {
                    sequence,
                    ..request()
                },
                path([2; 6]),
                COOLDOWN_US + 10,
            )
            .unwrap();
        }
        assert_eq!(
            b.receive(
                &hw,
                Record {
                    sequence: 9,
                    ..request()
                },
                path([2; 6]),
                COOLDOWN_US + 11
            ),
            Err(ERR_NOMEM)
        );
        b.receive(
            &hw,
            request(),
            Path {
                now: true,
                ..path([2; 6])
            },
            COOLDOWN_US + 12,
        )
        .unwrap();
        assert_eq!(f.holds.len(), 8);
        b.receive(
            &hw,
            Record {
                session: 18,
                ..request()
            },
            path([2; 6]),
            RETAIN_US + 11,
        )
        .unwrap();
        assert_eq!(f.holds.len(), 9);
        b.expire(2 * RETAIN_US + 12);
        assert!(b.seen.iter().all(Option::is_none));
    }
    #[test]
    fn disabled_or_failed_hold_does_not_reserve_a_reverse_route() {
        let mut f = Fake {
            reject: ERR_BUSY,
            ..Default::default()
        };
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut b = node([4; 6]);
        assert_eq!(b.receive(&hw, request(), path([2; 6]), 1), Err(ERR_BUSY));
        assert!(b.seen.iter().all(Option::is_none));
        b.enabled = false;
        assert_eq!(b.receive(&hw, request(), path([2; 6]), 2), Err(ERR_ARG));
        assert!(f.holds.is_empty());
    }
    unsafe extern "C" fn now(_: *mut c_void) -> u64 {
        1_000_000
    }
    unsafe extern "C" fn alarm(
        _: *mut c_void,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: i32,
        _: i32,
    ) -> i32 {
        1
    }
    unsafe extern "C" fn cancel(_: *mut c_void, _: i32) -> i32 {
        0
    }
    unsafe extern "C" fn release(p: *mut c_void, ptr: *mut c_void) -> i32 {
        (*(p as *mut Fake)).released += 1;
        std::alloc::dealloc(ptr.cast(), core::alloc::Layout::new::<Routing>());
        0
    }
    #[test]
    fn admission_waits_for_matching_ack_and_disable_reclaims_routing_allocation() {
        let mut f = Fake::default();
        let mut h = host(&mut f);
        h.now_us = Some(now);
        h.alarm_set = Some(alarm);
        h.alarm_cancel = Some(cancel);
        h.mem_free = Some(release);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let ptr = std::boxed::Box::into_raw(std::boxed::Box::new(Routing::new([2; 6], 17)));
        let mut cx = Context {
            record: [0; announce::INFO_MAX],
            len: 0,
            channel: 6,
            next_id: 0,
            pending: None,
            relay_links: [0; 12],
            relay_alarm: 0,
            routing: Some(ptr as usize),
        };
        let reply = Reply {
            from: 52,
            token: 9,
            method: 12,
            wanted: true,
        };
        handle(&hw, &mut cx, 12, &[0xa1, 1, 0xf5], reply).unwrap();
        assert!(!unsafe { (*ptr).enabled });
        assert_eq!(f.messages[0], (46, 46, radio::SUBSCRIBE, 1));
        let mut ack = CallView {
            component: 46,
            method: radio::SUBSCRIBE,
            value: 0,
            flags: 0,
            data: &[0xa0],
            time_us: 0,
            pkt: 0,
            from: 46,
            id: 2,
            request_id: None,
            src: None,
        };
        event(&hw, &mut cx, &ack);
        assert!(!unsafe { (*ptr).enabled });
        ack.id = 1;
        event(&hw, &mut cx, &ack);
        assert!(unsafe { (*ptr).enabled });
        assert_eq!(f.messages.len(), 2);
        event(&hw, &mut cx, &ack);
        assert_eq!(f.messages.len(), 2);
        handle(&hw, &mut cx, 12, &[0xa1, 1, 0xf4], reply).unwrap();
        ack.id = 2;
        event(&hw, &mut cx, &ack);
        assert!(cx.routing.is_none());
        assert_eq!(f.released, 1);
        assert!(f.packets.is_empty());
    }
    #[test]
    fn delayed_old_request_reply_cannot_replace_a_newer_candidate() {
        let mut r = node([2; 6]);
        let q = Record {
            kind: REPLY,
            ..request()
        };
        r.learn(Record { sequence: 2, ..q }, path([4; 6]), 200, 100)
            .unwrap();
        r.learn(q, path([8; 6]), 300, 50).unwrap();
        assert_eq!(r.routes[0].unwrap().sequence, 2);
        r.expire(RETAIN_US + 201);
        assert!(r.routes.iter().all(Option::is_none));
    }
}
