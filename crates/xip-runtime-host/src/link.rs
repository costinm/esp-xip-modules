//! The `link` component of the host-runtime. The same rules as the device runtimes (`xip_runtime.c`): named links with a provider
//! (a native one the test plays, or a module) and subscribers; frames delivered as packets with `CALL_LINK`, or forwarded by a
//! route before anyone sees them; accepted TX transfers ownership to its provider;
//! READY/STATE events remain; no sender or completion ledger is kept.
use std::ffi::{c_char, c_int, c_void, CStr};
use std::rc::Rc;
use xip_abi::*;

use crate::hostcalls::ctx;
use crate::quic::{self, QuicClass, QuicInfo, QuicKind, QuicPacket};
use crate::runtime::{deliver, route, Inst, QItem, Runtime, Shared};

const LINKS_MAX: usize = 12;
const SUBS_MAX: usize = 6;
const ROUTES_MAX: usize = 16;

/// How a native link takes a frame, for tests: the answer of the driver's transmit function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeTx {
    /// Already sent (`XIP_LINK_TX_DONE`).
    #[default]
    Done,
    /// Accepted; the test completes it later with [`Runtime::native_tx_done`].
    Async,
    /// Refused with `ERR_BUSY`: the sender keeps the packet and waits for `READY`.
    Busy,
}

#[derive(Default)]
struct Route {
    key: Vec<u8>,
    egress: u32,
    token: u64,
    flags: u32,
}

#[derive(Default)]
pub(crate) struct Entry {
    name: String,
    provider: u32,
    owner: i32,
    tx_component: i32,
    tx_method: i32,
    mtu: u32,
    rate_kbps: u32,
    flags: u32,
    state: u32,
    received: u32,
    sent: u32,
    forwarded: u32,
    dropped: u32,
    /// (owner component, component, method) of each subscriber
    subs: Vec<(i32, i32, i32)>,
    key_offset: usize,
    key_len: usize,
    routes: Vec<Route>,
    mode: NativeTx,
    /// What was sent to a native link: (destination token, frame).
    native_tx: Vec<(u64, Vec<u8>)>,
    /// Slots accepted by a native link and not completed.
    inflight: std::collections::VecDeque<i32>,
}

#[derive(Default)]
pub(crate) struct Links {
    list: Vec<Entry>,
    /// The common QUIC receive function (`Runtime::set_quic_rx`): frames of port-less links are offered to it first.
    quic_rx: Option<QuicRx>,
    /// The classifier in front of it: local callback, relay table, counters, reply notes (`quic.rs`).
    pub(crate) quic: crate::quic::QuicState,
}

/// `(link name, source token, signal, payload) -> taken`.
pub type QuicRx = std::rc::Rc<std::cell::RefCell<dyn FnMut(&str, u64, i32, &[u8]) -> bool>>;

impl Links {
    fn find(&self, name: &str) -> Option<usize> {
        self.list.iter().position(|e| e.name == name)
    }
    fn named(&mut self, name: &str) -> Option<usize> {
        if name.is_empty() || name.len() > 15 {
            return None;
        }
        if let Some(at) = self.find(name) {
            return Some(at);
        }
        if let Some(at) = self.list.iter().position(|e| e.name.is_empty()) {
            self.list[at] = Entry { name: name.to_owned(), ..Default::default() };
            return Some(at);
        }
        if self.list.len() >= LINKS_MAX { return None; }
        self.list.push(Entry { name: name.to_owned(), ..Default::default() });
        Some(self.list.len() - 1)
    }
    fn at(&mut self, addr: u32) -> Option<&mut Entry> {
        if addr < ADDR_LINK {
            return None;
        }
        self.list.get_mut((addr - ADDR_LINK) as usize).filter(|e| !e.name.is_empty())
    }
}

fn addr_of(index: usize) -> u32 {
    ADDR_LINK + index as u32
}
unsafe fn name_of(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}
fn component_of(me: &Rc<Inst>) -> i32 {
    me.st.borrow().component.unwrap_or(0)
}
fn now_us(sh: &Shared) -> u32 {
    (sh.st.borrow().now.wrapping_mul(1000)) as u32
}
fn name_err(name: &str) -> c_int {
    if name.is_empty() || name.len() > 15 { ERR_ARG } else { ERR_NOMEM }
}

unsafe extern "C" fn open(user: *mut c_void, name: *const c_char, component: c_int, method: c_int) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    let Some(name) = (unsafe { name_of(name) }) else { return ERR_ARG };
    let mut links = sh.links.borrow_mut();
    let Some(at) = links.named(&name) else { return name_err(&name) };
    let entry = &mut links.list[at];
    if !entry.subs.contains(&(owner, component, method)) {
        if entry.subs.len() >= SUBS_MAX {
            return ERR_NOMEM;
        }
        entry.subs.push((owner, component, method));
    }
    addr_of(at) as c_int
}

/// TODO: the host-runtime should share the receive path of the C runtime (`xip_ppp.c`: the PPP decoder that writes a record straight into a
/// pool packet) instead of binding by subscription and injecting whole frames, so the tests exercise the decoder, the
/// packet layout (headroom in front) and the drops of the real path; and a fake TCP/UDP listener for the manifest's stream and datagram ports (`PORT_NET + stream`), so `mod_test` (datagram port 3, stream port 3: an echo on both) and the console run end to end in the host-runtime against a Rust server or client on the host; once `send` and routing exist `mod_test` becomes the e2e test between two devices.
///
/// The port of a manifest (key 9) for a module: subscribe it to the link of that port, as the C runtime does when it creates the instance.
pub(crate) fn bind_port(sh: &Shared, me: &Rc<Inst>, port: u32) -> c_int {
    if port != PORT_CONSOLE {
        return ERR_UNSUPPORTED;
    }
    let owner = component_of(me);
    let mut links = sh.links.borrow_mut();
    let Some(at) = links.named("serial") else { return ERR_NOMEM };
    let entry = &mut links.list[at];
    if !entry.subs.contains(&(owner, owner, 0)) {
        entry.subs.push((owner, owner, 0));
    }
    addr_of(at) as c_int
}

unsafe extern "C" fn close(user: *mut c_void, addr: u32) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    let mut links = sh.links.borrow_mut();
    let Some(entry) = links.at(addr) else { return ERR_NOTFOUND };
    entry.subs.retain(|s| s.0 != owner);
    if entry.provider == LINK_NONE && entry.subs.is_empty() { *entry = Entry::default(); }
    0
}

unsafe extern "C" fn provide(user: *mut c_void, name: *const c_char, component: c_int, method: c_int, mtu: u32) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    let Some(name) = (unsafe { name_of(name) }) else { return ERR_ARG };
    let mut links = sh.links.borrow_mut();
    let Some(at) = links.named(&name) else { return name_err(&name) };
    let entry = &mut links.list[at];
    if entry.provider == LINK_NATIVE || (entry.provider == LINK_MODULE && entry.owner != owner) {
        return ERR_BUSY;
    }
    entry.provider = LINK_MODULE;
    entry.owner = owner;
    entry.tx_component = component;
    entry.tx_method = method;
    entry.mtu = mtu;
    entry.state = LINK_DOWN;
    addr_of(at) as c_int
}

/// An event for every subscriber (READY, STATE), queued like any call.
fn event(sh: &Shared, addr: u32, kind: u32, value: i32, id: u64) {
    let subs = match sh.links.borrow_mut().at(addr) {
        Some(l) => l.subs.clone(),
        None => return,
    };
    for (owner, component, method) in subs {
        if method == LINK_EGRESS { continue; }
        if let Some(t) = route(sh, owner) {
            t.st.borrow_mut().queue.push_back(QItem {
                component,
                method,
                value,
                request: 4,
                from: addr,
                id,
                lflags: CALL_LINK | CALL_LINK_EVENT | (kind << 8),
                ..Default::default()
            });
        }
    }
}

/// The provider releases its accepted reference after physical use.
fn complete(sh: &Shared, slot: i32, outcome: i32) {
    let _=outcome;sh.pool.release(slot);
}

/// Send the frame in `slot` (one reference is the caller's) to the link: on 0 the runtime owns the reference, otherwise the caller
/// keeps it. Legacy completion routing arguments do not retain sender state.
pub(crate) fn link_tx(sh: &Shared, dst: u32, slot: i32, start: u16, len: u16, token: u64, sender: i32, component: i32, method: i32) -> i32 {
    let circuit = {
        let mut links=sh.links.borrow_mut();
        if links.at(dst).is_some_and(|l|l.name=="relay" && l.provider==LINK_NATIVE) {
            let cid=token.to_le_bytes();let n=quic::cid_class_len(cid[0]);
            Some(relay_select(&mut links,&cid[..n],sh.st.borrow().now*1000))
        } else {None}
    };
    if let Some(entry)=circuit {
        let Some(e)=entry else {return ERR_NOTFOUND};
        if !e.managed.as_ref().is_some_and(|(_,r,_)|r.endpoint!=0) {return ERR_NOTFOUND;}
        let prefix=e.out_cid.len()+2;
        if prefix>start as usize || len as usize+prefix>quic::MAX_PACKET {return ERR_SIZE;}
        unsafe {
            let p=sh.pool.slot(slot).add(start as usize-prefix);*p=0x40;
            std::ptr::copy_nonoverlapping(e.out_cid.as_ptr(),p.add(1),e.out_cid.len());*p.add(prefix-1)=0;
        }
        if !e.address.is_empty() {
            let bytes=unsafe {std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize-prefix),len as usize+prefix)};
            if let Some(rc)=crate::network::udp_tx_address(sh,e.egress,bytes,&e.address) {if rc==0 {sh.pool.release(slot);}return rc;}
        }
        return link_tx(sh,e.egress,slot,start-prefix as u16,len+prefix as u16,e.token,sender,component,method);
    }
    enum Target {
        None,
        Native(NativeTx),
        Module(i32, i32, i32),
    }
    let target = match sh.links.borrow_mut().at(dst) {
        Some(l) if l.provider == LINK_NATIVE => Target::Native(l.mode),
        Some(l) if l.provider == LINK_MODULE => Target::Module(l.owner, l.tx_component, l.tx_method),
        _ => Target::None,
    };
    if matches!(target, Target::None) {
        return ERR_NOTFOUND;
    }
    let _=(component,method);
    let rc = match target {
        Target::Native(NativeTx::Busy) => {
            ERR_BUSY
        }
        Target::Native(mode) => {
            let bytes = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize), len as usize) };
            if let Some(rc) = crate::network::udp_tx(sh, dst, bytes, token) {
                if rc == 0 { complete(sh, slot, 0); }
                return rc;
            }
            let frame = bytes.to_vec();
            {
                let mut links = sh.links.borrow_mut();
                let l = links.at(dst).unwrap();
                l.native_tx.push((token, frame));
                sh.notify.notify_one();
                if mode == NativeTx::Async {
                    l.inflight.push_back(slot);
                }
            }
            if mode == NativeTx::Done {
                complete(sh, slot, 0);
            }
            0
        }
        Target::Module(owner, tx_component, tx_method) => {
            let rc = match route(sh, owner) {
                Some(t) => deliver(sh, &t, slot, start, len, tx_component, tx_method, sender as u32, token, 0, CALL_LINK, 0, None),
                None => ERR_NOTFOUND,
            };
            rc
        }
        Target::None => unreachable!(),
    };
    if let Some(l) = sh.links.borrow_mut().at(dst) {
        if rc == 0 {
            l.sent += 1;
        } else if rc != ERR_BUSY {
            l.dropped += 1;
        }
    }
    rc
}

/// Additional manifest component ownership takes priority, then the component
/// module with a datagram/stream port or hosted table, then the console fallback.
pub(crate) fn request_target(sh:&Shared,component:i32)->Result<Rc<Inst>,i32>{
    let find=||sh.insts.borrow().iter().find(|i|{
        let st=i.st.borrow();!st.ended && (st.envelope_components.contains(&component) || st.component==Some(component))
    }).cloned();
    let mut owner=find();
    if owner.is_none(){
        let starter=sh.request_starter.borrow().clone();
        if let Some(start)=starter {
            match start(component) {
                Ok(inst)=>{crate::runtime::drain(sh,&inst.me);if inst.me.st.borrow().ended{let rc=inst.me.st.borrow().result;return Err(if rc<0{rc}else{ERR_NOTFOUND});}owner=find();},
                Err(ERR_NOTFOUND)=>{},Err(error)=>return Err(error),
            }
        }
    }
    let owner=owner.or_else(||route(sh,QUIC_CONSOLE_COMPONENT)).ok_or(ERR_NOTFOUND)?;
    if !owner.st.borrow().start_done {
        if !owner.st.borrow().queue.iter().any(|c|c.component==HOST_COMPONENT as i32 && c.method==HOST_START as i32){crate::runtime::enqueue(&owner,HOST_COMPONENT as i32,HOST_START as i32,0);}
        crate::runtime::drain(sh,&owner);
    }
    let state=owner.st.borrow();if state.ended{return Err(if state.result<0{state.result}else{ERR_NOTFOUND});}drop(state);
    Ok(owner)
}

/// `send(dst = link address)`. `ERR_BUSY` is the one failure that leaves the packet with the sender.
pub(crate) fn send(sh: &Shared, me: &Rc<Inst>, pkt: u32, len: usize, dst: u32, component: i32, method: i32, token: u64) -> i32 {
    let held = sh.pool.get(pkt);
    let Some(e) = held else {
        return ERR_ARG;
    };
    let slot = e.slot as i32;
    if len > PKT_MAX as usize {
        sh.pool.release(slot);
        return ERR_SIZE;
    }
    // The answer to an envelope request is wrapped here: {1: component, 2: method, 3: id, 6: the module's map} or, for a bare negative
    // integer, {.., 7: error}. The head goes in front of the payload at the same start (the payload moves up); BUSY takes it off again.
    let olen = len;
    let more = method & SEND_MORE != 0; // another answer to an open envelope request (a subscription's events)
    let method = method & !SEND_MORE;
    let env = me.st.borrow().env.find(dst, token, component, method);
    let (len, env_head) = match env {
        Some(p) => {
            let at = unsafe { std::slice::from_raw_parts_mut(sh.pool.slot(slot).add(e.start as usize), len) };
            let failed = crate::envelope::is_error(at);
            let head = crate::envelope::head(p.component, p.method, p.id, failed.unwrap_or(0));
            if len + head.len() > PKT_MAX as usize || e.start as usize + len + head.len() > sh.pool.slot_size() as usize {
                if !more {
                    me.st.borrow_mut().env.done(&p);
                }
                sh.pool.release(slot);
                return ERR_SIZE;
            }
            unsafe {
                let base = sh.pool.slot(slot).add(e.start as usize);
                std::ptr::copy(base, base.add(head.len()), len);
                std::ptr::copy_nonoverlapping(head.as_ptr(), base, head.len());
            }
            (if failed.is_some() { head.len() } else { len + head.len() }, head.len())
        }
        None => (len, 0),
    };
    let (tx_start, tx_len, rid) = match quic_reply_prefix(sh, dst, token, slot, e.start, len) {
        Ok(Some(w)) => w,
        Ok(None) => (e.start, len as u16, 0),
        Err(rc) => {
            sh.pool.release(slot);
            return rc;
        }
    };
    let rc = link_tx(sh, dst, slot, tx_start, tx_len, token, component_of(me), component, method);
    if rc != ERR_BUSY && !more && (tx_start, tx_len) != (e.start, len as u16) {
        sh.links.borrow_mut().quic.done_reply(dst, token, rid);
    }
    if rc == ERR_BUSY {
        if tx_len as usize != len {
            // the sender keeps the packet and retries: take the header off again
            let hl = tx_len as usize - len;
            unsafe { let at = sh.pool.slot(slot).add(e.start as usize); std::ptr::copy(at.add(hl), at, len); }
        }
        if env_head != 0 {
            unsafe { let at = sh.pool.slot(slot).add(e.start as usize); std::ptr::copy(at.add(env_head), at, olen); }
        }
        return rc;
    }
    if let Some(p) = env.filter(|_| !more) {
        me.st.borrow_mut().env.done(&p);
    }
    if rc != 0 {
        sh.pool.release(slot);
    }
    rc
}

unsafe extern "C" fn tx_done(user: *mut c_void, addr: u32, pkt: u32, outcome: c_int) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    if !sh.links.borrow_mut().at(addr).is_some_and(|l| l.provider == LINK_MODULE && l.owner == owner) {
        return ERR_ARG;
    }
    let Some(e) = sh.pool.get(pkt) else { return ERR_ARG };
    complete(sh, e.slot as i32, outcome); // the runtime's: back to the sender, or released
    0
}

unsafe extern "C" fn ready(user: *mut c_void, addr: u32) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    if !sh.links.borrow_mut().at(addr).is_some_and(|l| l.provider == LINK_MODULE && l.owner == owner) {
        return ERR_ARG;
    }
    event(sh, addr, LINK_EVENT_READY, 0, 0);
    0
}

fn set_state(sh: &Shared, addr: u32, state: u32, rate_kbps: u32, owner: Option<i32>) -> i32 {
    let changed = {
        let mut links = sh.links.borrow_mut();
        let Some(l) = links.at(addr) else { return ERR_ARG };
        if owner.is_some_and(|o| !(l.provider == LINK_MODULE && l.owner == o)) {
            return ERR_ARG;
        }
        let changed = l.state != state || l.rate_kbps != rate_kbps;
        l.state = state;
        l.rate_kbps = rate_kbps;
        changed
    };
    if changed {
        event(sh, addr, LINK_EVENT_STATE, state as i32, rate_kbps as u64);
    }
    0
}
unsafe extern "C" fn set_state_c(user: *mut c_void, addr: u32, state: u32, rate_kbps: u32) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    set_state(sh, addr, state, rate_kbps, Some(component_of(&me)))
}

/// A frame that matches a route is sent on from here, before any module is woken. True when it was consumed.
fn link_route(sh: &Shared, addr: u32, slot: i32, start: u16, len: u16) -> bool {
    let hit = {
        let mut links = sh.links.borrow_mut();
        let Some(l) = links.at(addr) else { return false };
        if l.key_len == 0 || (len as usize) < l.key_offset + l.key_len {
            return false;
        }
        let key = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize + l.key_offset), l.key_len) };
        l.routes.iter().find(|r| r.key == key).map(|r| (r.egress, r.token, r.flags))
    };
    let Some((egress, token, flags)) = hit else { return false };
    let copy = flags & ROUTE_COPY != 0;
    if copy {
        sh.pool.retain(slot); // one reference for the forward, one for the subscribers
    }
    let rc = link_tx(sh, egress, slot, start, len, token, 0, 0, 0);
    if let Some(l) = sh.links.borrow_mut().at(addr) {
        if rc == 0 { l.forwarded += 1 } else { l.dropped += 1 }
    }
    if rc != 0 {
        sh.pool.release(slot); // the forward's reference: nobody took it
    }
    !copy
}

/// The console module answers the envelopes that no other module serves (`QUIC_CONSOLE_COMPONENT`).
const QUIC_CONSOLE_COMPONENT: i32 = 20;

fn quic_active(links: &Links) -> bool {
    links.quic_rx.is_some() || links.quic.local.is_some() || links.quic.classifier
}

/// The common QUIC receive function, the classifier part: true when the packet went to its one handler or was dropped and counted (the caller
/// must not touch `slot`), false when it was declined (not a QUIC packet, or not a QUIC link) and routing goes on.
fn quic_common_rx(sh: &Shared, addr: u32, slot: i32, start: u16, len: u16, source: u64, signal: i32, time_us: u32) -> bool {
    let (name, short_cid_len, local) = {
        let mut links = sh.links.borrow_mut();
        if len == 0 || !links.quic.classifier {
            return false;
        }
        let Some(name) = links.at(addr).map(|l| l.name.clone()) else { return false };
        if !links.quic.is_quic_link(&name) {
            return false;
        }
        (name, links.quic.short_cid_len, links.quic.local.clone())
    };
    let data = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize), len as usize) };
    let (class, h) = quic::classify(data, short_cid_len);
    let stat = |f: fn(&mut quic::QuicStats) -> &mut u32| {
        let mut links = sh.links.borrow_mut();
        *f(&mut links.quic.stats) += 1;
    };
    stat(|s| &mut s.rx);
    match class {
        QuicClass::NotQuic => {
            stat(|s| &mut s.declined);
            return false;
        }
        QuicClass::Malformed => {
            stat(|s| &mut s.malformed);
            sh.pool.release(slot);
            return true;
        }
        _ => {}
    }
    if len as usize > quic::MAX_PACKET {
        stat(|s| &mut s.oversize);
        sh.pool.release(slot);
        return true;
    }
    let info = QuicInfo { link: name, source, signal, header: h.clone() };
    let give = |local: &quic::QuicLocal| (local.borrow_mut())(&info, QuicPacket { slot, start, len });
    if class == QuicClass::Short {
        let entry = relay_select(&mut sh.links.borrow_mut(),&h.dcid,sh.st.borrow().now*1000);
        if entry.as_ref().is_some_and(|e|e.managed.as_ref().is_some_and(|(_,r,_)|r.endpoint!=0)) {
            let relay=sh.links.borrow().find("relay").map(addr_of);
            let prefix=h.dcid.len()+2;
            if let Some(relay)=relay.filter(|relay|*relay!=addr && h.first&3==0 && len as usize>prefix) {
                let mut cid=[0u8;8];cid[..h.dcid.len()].copy_from_slice(&h.dcid);
                if !quic_common_rx(sh,relay,slot,start+prefix as u16,len-prefix as u16,u64::from_le_bytes(cid),signal,time_us) {sh.pool.release(slot);}
            } else {sh.pool.release(slot);}
            return true;
        }
        match (entry, &local) {
            (None, _) => stat(|s| &mut s.short_unknown),
            (Some(e), _) if e.kind == QuicKind::Relay => {
                // forward or drop, no queue: the DCID is replaced in the empty headroom, the same pool packet goes to the egress link and peer
                let at = unsafe { sh.pool.slot(slot).add(start as usize) };
                let shift = unsafe { quic::short_rewrite(at, len as usize, &h, &e.out_cid, start as usize) };
                let rc = match shift {
                    Ok(d) => {
                        let start=(start as isize+d) as u16;let len=(len as isize-d) as u16;
                        let result=if !e.address.is_empty() {
                            let bytes=unsafe {std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize),len as usize)};
                            crate::network::udp_tx_address(sh,e.egress,bytes,&e.address)
                        } else {None};
                        match result {Some(rc)=>{if rc==0 {sh.pool.release(slot);}rc},None=>link_tx(sh,e.egress,slot,start,len,e.token,0,0,0)}
                    },
                    Err(rc) => rc,
                };
                if rc == 0 {
                    stat(|s| &mut s.short_relay);
                    return true;
                }
                stat(|s| &mut s.short_relay_failed);
            }
            (Some(_), None) => stat(|s| &mut s.short_no_handler),
            (Some(_), Some(local)) => {
                stat(|s| &mut s.short_local);
                give(local);
                return true;
            }
        }
        sh.pool.release(slot);
        return true;
    }
    if h.version != quic::VERSION_CBOR {
        match &local {
            Some(local) => {
                stat(|s| &mut s.long_other);
                give(local);
            }
            None => {
                stat(|s| &mut s.long_other_dropped);
                sh.pool.release(slot);
            }
        }
        return true;
    }
    // a CBOR datagram: the envelope follows the SCID; routed like an envelope that arrives on a module's port
    let envelope = &data[h.body..];
    let Some((component, _method, _id, response)) = (!envelope.is_empty()).then(|| quic::envelope(envelope)).flatten() else {
        stat(|s| &mut s.long_cbor_bad);
        sh.pool.release(slot);
        return true;
    };
    if response {
        // an answer (field 6 or 7): to the local callback, which matches it by id, else dropped; never dispatched as a request
        stat(|s| &mut s.long_cbor_response);
        match &local {
            Some(local) => give(local),
            None => sh.pool.release(slot),
        }
        return true;
    }
    // a module that has a datagram port in its manifest (key 9) gets its envelopes like the ones that arrive on that port
    let t=match request_target(sh,component) {
        Ok(t)=>t,
        Err(error)=>{
            stat(|s|&mut s.long_cbor_no_target);
            if let Some(id)=_id {
                if let Some(mut reply)=quic::long_build(quic::VERSION_CBOR,&h.scid,&h.dcid){
                    reply.extend(crate::envelope::head(component,_method,id,error));
                    if reply.len()<=PKT_MAX as usize {
                        unsafe{std::ptr::copy_nonoverlapping(reply.as_ptr(),sh.pool.slot(slot).add(start as usize),reply.len());}
                        if link_tx(sh,addr,slot,start,reply.len() as u16,source,0,0,0)==0{return true;}
                    }
                }
            }
            sh.pool.release(slot);return true;
        }
    };
    let now_ms = sh.st.borrow().now;
    sh.links.borrow_mut().quic.note_reply(addr, source, _id.unwrap_or(0), &h, now_ms);
    let target = component_of(&t);
    if deliver(sh, &t, slot, start + h.body as u16, (len as usize - h.body) as u16, target, 0, addr, source, signal, CALL_LINK | CALL_ENVELOPE, time_us, None) != 0 {
        let mut links = sh.links.borrow_mut();
        links.quic.stats.long_cbor_no_target += 1; // its inbox is full
        links.quic.done_reply(addr, source, _id.unwrap_or(0));
        drop(links);
        sh.pool.release(slot);
        return true;
    }
    stat(|s| &mut s.long_cbor);
    true
}

/// The answer to a CBOR datagram goes back as one: the long header (the request's CIDs swapped) is written into the empty headroom.
/// Ok(None): no note for (link, token); Ok(Some((start, len))): prefixed; Err: it does not fit.
fn quic_reply_prefix(sh: &Shared, dst: u32, token: u64, slot: i32, start: u16, len: usize) -> Result<Option<(u16, u16, u64)>, i32> {
    // the answer must carry the request's id (field 3; absent = 0)
    let payload = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize), len) };
    let id = xip_abi::cbor::get_int(payload, 3).ok().flatten().filter(|i| *i >= 0).unwrap_or(0) as u64;
    let now_ms = sh.st.borrow().now;
    let header = {
        let links = sh.links.borrow();
        links.quic.find_reply(dst, token, id, now_ms).and_then(|r| quic::long_build(quic::VERSION_CBOR, &r.dcid, &r.scid))
    };
    let found = sh.links.borrow().quic.find_reply(dst, token, id, now_ms).is_some();
    if !found {
        return Ok(None);
    }
    let Some(header) = header.filter(|h| len + h.len() <= PKT_MAX as usize) else {
        sh.links.borrow_mut().quic.done_reply(dst, token, id);
        return Err(ERR_SIZE);
    };
    // the header goes in front of the payload at the same start (links need their headroom and alignment): the payload moves up
    unsafe {
        let at = sh.pool.slot(slot).add(start as usize);
        std::ptr::copy(at, at.add(header.len()), len);
        std::ptr::copy_nonoverlapping(header.as_ptr(), at, header.len());
    }
    Ok(Some((start, (len + header.len()) as u16, id)))
}

/// The packet in `slot` (one reference is the caller's) goes to the route or to every subscriber: one reference each.
fn fanout(sh: &Shared, addr: u32, slot: i32, start: u16, len: u16, source: u64, signal: i32, time_us: u32) {
    if let Some(l) = sh.links.borrow_mut().at(addr) {
        l.received += 1;
    }
    // the classifier: exactly one handler gets a packet of a QUIC link (the local callback, a module, or the pool again)
    if quic_common_rx(sh, addr, slot, start, len, source, signal, time_us) {
        return;
    }
    // ownership, not fan-out: the QUIC receive function takes the frame (the packet returns to the pool here) or routing goes on
    let app = sh.links.borrow().quic_rx.clone();
    if let Some(app) = app {
        let name = sh.links.borrow_mut().at(addr).map(|l| l.name.clone()).unwrap_or_default();
        let payload = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize), len as usize) };
        if len != 0 && (app.borrow_mut())(&name, source, signal, payload) {
            sh.pool.release(slot);
            return;
        }
    }
    if link_route(sh, addr, slot, start, len) {
        return;
    }
    let subs: Vec<_> = sh.links.borrow_mut().at(addr).map(|l| l.subs.iter().copied().filter(|s| s.2 != LINK_EGRESS).collect()).unwrap_or_default();
    if subs.is_empty() {
        sh.pool.release(slot);
        return;
    }
    let mut lost = 0;
    for (k, (owner, component, method)) in subs.iter().enumerate() {
        if k + 1 < subs.len() {
            sh.pool.retain(slot);
        }
        let ok = route(sh, *owner).is_some_and(|t| deliver(sh, &t, slot, start, len, *component, *method, addr, source, signal, CALL_LINK, time_us, None) == 0);
        if !ok {
            sh.pool.release(slot);
            lost += 1;
        }
    }
    if lost != 0 {
        if let Some(l) = sh.links.borrow_mut().at(addr) {
            l.dropped += lost;
        }
    }
}

unsafe extern "C" fn rx(user: *mut c_void, addr: u32, pkt: u32, len: usize, source: u64, signal: i32, time_us: u32) -> c_int {
    let (sh, me) = unsafe { ctx(user) };
    let owner = component_of(&me);
    let Some(e) = sh.pool.get(pkt) else { return ERR_ARG };
    if !sh.links.borrow_mut().at(addr).is_some_and(|l| l.provider == LINK_MODULE && l.owner == owner) {
        sh.pool.release(e.slot as i32);
        return ERR_ARG;
    }
    if len > PKT_MAX as usize {
        sh.pool.release(e.slot as i32);
        return ERR_SIZE;
    }
    fanout(sh, addr, e.slot as i32, e.start, len as u16, source, signal, if time_us != 0 { time_us } else { now_us(sh) });
    0
}

unsafe extern "C" fn info(user: *mut c_void, addr: u32, out: *mut LinkInfo) -> c_int {
    let (sh, _me) = unsafe { ctx(user) };
    if out.is_null() || unsafe { (*out).size } < std::mem::size_of::<LinkInfo>() as u32 {
        return ERR_ARG;
    }
    let mut links = sh.links.borrow_mut();
    let Some(l) = links.at(addr) else { return ERR_NOTFOUND };
    unsafe { *out = l.describe() };
    0
}

impl Entry {
    fn describe(&self) -> LinkInfo {
        let mut i = LinkInfo::default();
        i.provider = self.provider;
        i.state = self.state;
        i.mtu = self.mtu;
        i.rate_kbps = self.rate_kbps;
        i.flags = self.flags;
        i.subscribers = self.subs.iter().filter(|s| s.2 != LINK_EGRESS).count() as u32;
        i.received = self.received;
        i.sent = self.sent;
        i.forwarded = self.forwarded;
        i.dropped = self.dropped;
        i.name[..self.name.len()].copy_from_slice(self.name.as_bytes());
        i
    }
}

unsafe extern "C" fn classify(user: *mut c_void, addr: u32, offset: u32, len: u32) -> c_int {
    let (sh, _me) = unsafe { ctx(user) };
    if len == 0 || len > 16 || offset > 255 {
        return ERR_ARG;
    }
    let mut links = sh.links.borrow_mut();
    let Some(l) = links.at(addr) else { return ERR_NOTFOUND };
    if l.key_len != len as usize || l.key_offset != offset as usize {
        l.key_len = len as usize;
        l.key_offset = offset as usize;
        l.routes.clear();
    }
    0
}

unsafe extern "C" fn route_c(user: *mut c_void, addr: u32, key: *const u8, key_len: usize, egress: u32, token: u64, flags: u32) -> c_int {
    let (sh, _me) = unsafe { ctx(user) };
    let mut links = sh.links.borrow_mut();
    let egress_ok = links.at(egress).is_some() && egress != addr;
    let Some(l) = links.at(addr) else { return ERR_NOTFOUND };
    if key.is_null() || l.key_len == 0 || key_len != l.key_len || !egress_ok {
        return ERR_ARG;
    }
    let key = unsafe { std::slice::from_raw_parts(key, key_len) }.to_vec();
    if let Some(r) = l.routes.iter_mut().find(|r| r.key == key) {
        r.egress = egress;
        r.token = token;
        r.flags = flags;
        return 0;
    }
    if l.routes.len() >= ROUTES_MAX {
        return ERR_NOMEM;
    }
    l.routes.push(Route { key, egress, token, flags });
    0
}

unsafe extern "C" fn unroute(user: *mut c_void, addr: u32, key: *const u8, key_len: usize) -> c_int {
    let (sh, _me) = unsafe { ctx(user) };
    let mut links = sh.links.borrow_mut();
    let Some(l) = links.at(addr) else { return ERR_NOTFOUND };
    if key.is_null() || key_len != l.key_len {
        return ERR_NOTFOUND;
    }
    let key = unsafe { std::slice::from_raw_parts(key, key_len) };
    let before = l.routes.len();
    l.routes.retain(|r| r.key != key);
    if l.routes.len() == before { ERR_NOTFOUND } else { 0 }
}

/// A module ended: its subscriptions go, its links lose their provider, and the frames it was holding for senders fail.
pub(crate) fn release(sh: &Shared, component: i32) {
    let mut down = Vec::new();
    {
        let mut links = sh.links.borrow_mut();
        links.quic.table.retain(|_,e| !e.managed.as_ref().is_some_and(|(owner,_,_)| *owner==component as usize));
        relay_gc(&mut links);
        for (i, l) in links.list.iter_mut().enumerate() {
            l.subs.retain(|s| s.0 != component);
            if l.provider == LINK_MODULE && l.owner == component {
                l.provider = LINK_NONE;
                l.owner = 0;
                l.state = LINK_DOWN;
                l.routes.clear();
                down.push(addr_of(i));
            }
        }
    }
    for addr in down {
        event(sh, addr, LINK_EVENT_STATE, LINK_DOWN as i32, 0);
    }
    for l in &mut sh.links.borrow_mut().list {
        if l.provider == LINK_NONE && l.subs.is_empty() { *l = Entry::default(); }
    }
}

static COMPONENT: Link = Link {
    version: LINK_VERSION,
    size: std::mem::size_of::<Link>() as u32,
    open: Some(open),
    close: Some(close),
    provide: Some(provide),
    rx: Some(rx),
    tx_done: Some(tx_done),
    ready: Some(ready),
    set_state: Some(set_state_c),
    info: Some(info),
    classify: Some(classify),
    route: Some(route_c),
    unroute: Some(unroute),
};
pub(crate) fn component() -> *const Link {
    &COMPONENT
}

/// The runtime side of the link API, played by a test: a driver that the runtime owns.
impl Runtime {
    /// Make `name` a link provided by the runtime (a driver the test plays). Frames modules send to it are kept for
    /// [`Runtime::take_link_tx`]; `mode` says how the driver answers. Returns its address. The link is `LINK_UP`.
    pub fn add_native_link(&self, name: &str, mtu: u32, mode: NativeTx) -> u32 {
        let addr = {
            let mut links = self.sh.links.borrow_mut();
            let at = links.named(name).expect("room for another link");
            let entry = &mut links.list[at];
            entry.provider = LINK_NATIVE;
            entry.mtu = mtu;
            entry.mode = mode;
            entry.state = LINK_UP;
            addr_of(at)
        };
        addr
    }

    /// Change how the native link `name` answers a transmit.
    pub fn set_native_mode(&self, name: &str, mode: NativeTx) {
        let mut links = self.sh.links.borrow_mut();
        if let Some(at) = links.find(name) {
            links.list[at].mode = mode;
        }
    }

    /// The native link `name` finished its oldest accepted frame (`NativeTx::Async`).
    pub fn native_tx_done(&self, name: &str, outcome: i32) {
        let slot = {
            let mut links = self.sh.links.borrow_mut();
            links.find(name).and_then(|at| links.list[at].inflight.pop_front())
        };
        if let Some(slot) = slot {
            complete(&self.sh, slot, outcome);
        }
    }

    /// The native link `name` has room again: its subscribers get `LINK_EVENT_READY`.
    pub fn native_ready(&self, name: &str) {
        let at = self.sh.links.borrow().find(name);
        if let Some(at) = at {
            event(&self.sh, addr_of(at), LINK_EVENT_READY, 0, 0);
        }
    }

    /// The state and rate of the native link `name` changed.
    pub fn native_set_state(&self, name: &str, state: u32, rate_kbps: u32) {
        let at = self.sh.links.borrow().find(name);
        if let Some(at) = at {
            set_state(&self.sh, addr_of(at), state, rate_kbps, None);
        }
    }

    /// The common QUIC receive function: every frame of a link without a port is offered to it first, with the link's name, the source token,
    /// the signal and the payload; returning true takes the frame (nobody else gets it), false lets routing go on. `None` clears.
    pub fn set_quic_rx(&self, rx: Option<impl FnMut(&str, u64, i32, &[u8]) -> bool + 'static>) {
        self.sh.links.borrow_mut().quic_rx = rx.map(|f| std::rc::Rc::new(std::cell::RefCell::new(f)) as QuicRx);
    }

    /// Switch the classifier on (off by default, so a stack that has not registered its DCIDs keeps its packets).
    pub fn set_quic_classifier(&self, on: bool) {
        self.sh.links.borrow_mut().quic.classifier = on;
    }
    /// The local-termination callback of the classifier (the QUIC stack of `fw/`): short packets whose DCID is a [`QuicKind::Local`] entry of
    /// the relay table, and long packets of versions other than 0x444d0002, reach it with the pool packet, which it owns and must give back
    /// ([`Runtime::quic_release`]) or pass on ([`Runtime::quic_send`]). `None` clears.
    pub fn set_quic_local(&self, f: Option<impl FnMut(&QuicInfo, QuicPacket) + 'static>) {
        self.sh.links.borrow_mut().quic.local = f.map(|f| std::rc::Rc::new(std::cell::RefCell::new(f)) as quic::QuicLocal);
    }
    /// The bytes of a packet the callback owns.
    pub fn quic_bytes(&self, p: &QuicPacket) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(self.sh.pool.slot(p.slot).add(p.start as usize), p.len as usize) }.to_vec()
    }
    /// Return the packet to the pool.
    pub fn quic_release(&self, p: QuicPacket) {
        self.sh.pool.release(p.slot);
    }
    /// Send the packet on the link `name` as it is. Consumed on success and on every failure except `ERR_BUSY` (returned in `Err` with the packet).
    pub fn quic_send(&self, p: QuicPacket, name: &str, token: u64) -> Result<(), (i32, Option<QuicPacket>)> {
        let sh = &*self.sh;
        let Some(at) = sh.links.borrow().find(name) else {
            sh.pool.release(p.slot);
            return Err((ERR_NOTFOUND, None));
        };
        match link_tx(sh, addr_of(at), p.slot, p.start, p.len, token, 0, 0, 0) {
            0 => Ok(()),
            ERR_BUSY => Err((ERR_BUSY, Some(p))),
            rc => {
                sh.pool.release(p.slot);
                Err((rc, None))
            }
        }
    }
    /// The address of the link `name` (0 when there is none), as `xip_link_find` reports it.
    pub fn link_addr(&self, name: &str) -> u32 {
        self.sh.links.borrow().find(name).map_or(0, addr_of)
    }
    /// The relay table (runtime-issued IDs): `cid` is 1 to 20 bytes as they appear in a short header (quic-lite: with the class tag).
    pub fn quic_table_add(&self, cid: &[u8], kind: QuicKind, egress: u32, token: u64, out_cid: &[u8]) -> i32 {
        let mut links = self.sh.links.borrow_mut();
        let t = &mut links.quic.table;
        if cid.is_empty() || cid.len() > quic::CID_MAX {
            return ERR_ARG;
        }
        if !t.contains_key(cid) && t.len() >= quic::TABLE_MAX {
            return ERR_NOMEM;
        }
        if kind == QuicKind::Relay && (out_cid.is_empty() || out_cid.len() > quic::CID_MAX) {
            return ERR_ARG;
        }
        t.insert(cid.to_vec(), quic::QuicEntry { last_used_us:0,idle_ms:0,address: Vec::new(), managed: None, kind, egress, token, out_cid: out_cid.to_vec() });
        0
    }
    /// The runtime issues the IDs: the lowest free quic-lite connection ID (1 byte for the first 127 entries), added and returned. Freed IDs are reused.
    pub fn quic_table_alloc(&self, kind: QuicKind, egress: u32, token: u64, out_cid: &[u8]) -> Result<Vec<u8>, i32> {
        let mut v = 1u64;
        loop {
            let cid = quic::cid_encode(v).ok_or(ERR_NOMEM)?;
            if !self.sh.links.borrow().quic.table.contains_key(&cid) {
                let rc = self.quic_table_add(&cid, kind, egress, token, out_cid);
                return if rc == 0 { Ok(cid) } else { Err(rc) };
            }
            v += 1;
        }
    }
    pub fn quic_table_remove(&self, cid: &[u8]) -> i32 {
        if self.sh.links.borrow_mut().quic.table.remove(cid).is_some() { 0 } else { ERR_NOTFOUND }
    }
    pub fn quic_table_len(&self) -> usize {
        self.sh.links.borrow().quic.table.len()
    }
    pub fn quic_stats(&self) -> quic::QuicStats {
        self.sh.links.borrow().quic.stats
    }
    /// One more link (by name) whose frames are QUIC packets; `ble` and `udp` are by default.
    pub fn quic_link_enable(&self, name: &str) {
        self.sh.links.borrow_mut().quic.names.push(name.to_owned());
    }
    /// A fixed DCID length for short headers; 0 (default): the quic-lite class rule.
    pub fn set_quic_short_cid_len(&self, n: usize) {
        self.sh.links.borrow_mut().quic.short_cid_len = n;
    }

    /// A frame arrives on the native link `name`: copied into a pool packet, stamped now, routed or delivered to the subscribers
    /// (nothing happens when nobody listens). Returns the usual result.
    pub fn inject_frame(&self, name: &str, source: u64, signal: i32, data: &[u8]) -> i32 {
        let sh = &*self.sh;
        let Some(at) = sh.links.borrow().find(name) else { return ERR_NOTFOUND };
        let addr = addr_of(at);
        if data.len() > PKT_MAX as usize {
            return ERR_SIZE;
        }
        if !quic_active(&sh.links.borrow()) && sh.links.borrow_mut().at(addr).is_some_and(|l| l.subs.iter().all(|s| s.2 == LINK_EGRESS) && l.routes.is_empty()) {
            return 0;
        }
        let Some(slot) = sh.pool.acquire(0) else {
            if let Some(l) = sh.links.borrow_mut().at(addr) {
                l.dropped += 1;
            }
            return ERR_NOMEM;
        };
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), sh.pool.slot(slot).add(PKT_HEADROOM as usize), data.len()) };
        fanout(sh, addr, slot, PKT_HEADROOM as u16, data.len() as u16, source, signal, now_us(sh));
        0
    }

    /// The frames modules sent to the native link `name` since the last call: (destination token, frame).
    pub fn take_link_tx(&self, name: &str) -> Vec<(u64, Vec<u8>)> {
        let mut links = self.sh.links.borrow_mut();
        links.find(name).map(|at| std::mem::take(&mut links.list[at].native_tx)).unwrap_or_default()
    }

    /// What `Link::info` reports for the link `name`.
    pub fn link_info(&self, name: &str) -> Option<LinkInfo> {
        let links = self.sh.links.borrow();
        links.find(name).map(|at| links.list[at].describe())
    }

    /// Packets in the pool that are not free: after a test has settled this is the leak check.
    pub fn packets_in_use(&self) -> u16 {
        self.sh.pool.slots() - self.sh.pool.available()
    }
}

// Datagram endpoints own native links; reuse only retired datagram entries.
pub(crate) fn add_datagram_link(sh: &Shared) -> Result<u32, i32> {
    let mut links = sh.links.borrow_mut();
    let at = if let Some(i) = links.list.iter().position(|l| l.name.is_empty() || (l.provider == LINK_NONE && l.subs.is_empty() && l.name.starts_with("udp:"))) { i }
        else { if links.list.len() >= LINKS_MAX { return Err(ERR_NOMEM); } let i = links.list.len(); links.list.push(Entry::default()); i };
    links.list[at] = Entry { name: format!("udp:{at}"), provider: LINK_NATIVE, mtu: PKT_MAX, state: LINK_UP, mode: NativeTx::Done, ..Default::default() };
    Ok(addr_of(at))
}
pub(crate) fn remove_datagram_link(sh: &Shared, addr: u32) {
    let mut links = sh.links.borrow_mut();
    if let Some(l) = links.at(addr) {
        l.provider = LINK_NONE; l.state = LINK_DOWN; l.routes.clear();
        if l.subs.is_empty() { *l = Entry::default(); }
    }
    for l in &mut links.list { l.routes.retain(|r| r.egress != addr); }
}

unsafe extern "C" fn relay_set(user:*mut c_void,r:*const RelayRoute,mode:u32)->i32 {
    let (sh,me)=ctx(user);
    if r.is_null() || mode>1 {return ERR_ARG;}
    let r=*r;
    if r.dcid_len==0 || r.dcid_len>20 || r.hop_count==0 || r.hop_count>4 || r.reserved!=0 || r.endpoint>1 {return ERR_ARG;}
    let mut total=0u32;
    for h in &r.hops[..r.hop_count as usize] {
        if h.link==0 || h.cid_len==0 || h.cid_len>20 || (h.address_len!=0 && h.address_len!=22) || h.padding!=[0;4] {return ERR_ARG;}
        if h.address_len==22 && ((h.address[16]==0 && h.address[17]==0) || h.address[22..]!=[0,0]) {return ERR_ARG;}
        total+=h.weight as u32;
    }
    if total==0 {return ERR_ARG;}
    if r.endpoint!=0 && (r.dcid_len>8 || (r.dcid_len==1 && r.dcid[0]==0) || quic::cid_class_len(r.dcid[0])!=r.dcid_len as usize) {return ERR_ARG;}
    let mut links=sh.links.borrow_mut();let cid=&r.dcid[..r.dcid_len as usize];
    let t=&mut links.quic.table;
    if let Some(old)=t.get(cid) {
        if mode==0 || !old.managed.as_ref().is_some_and(|(owner,_,_)|*owner==component_of(&me) as usize) {return ERR_BUSY;}
    } else if mode==1 {return ERR_NOTFOUND;} else if t.len()>=quic::TABLE_MAX {return ERR_NOMEM;}
    if r.endpoint!=0 {
        let Some(at)=links.named("relay") else {return ERR_NOMEM;};
        if links.list[at].provider==LINK_MODULE {return ERR_BUSY;}
        let link=&mut links.list[at];link.provider=LINK_NATIVE;link.state=LINK_UP;link.mtu=(quic::MAX_PACKET-22) as u32;
    }
    let t=&mut links.quic.table;
    let h=&r.hops[0];
    let (last_used_us,idle_ms)=t.get(cid).map_or((0,0),|e|(e.last_used_us,e.idle_ms));
    t.insert(cid.to_vec(),quic::QuicEntry {last_used_us,idle_ms,address:Vec::new(),managed:Some((component_of(&me) as usize,r,0)),kind:QuicKind::Relay,
        egress:h.link,token:h.token,out_cid:h.rewritten_dcid[..h.cid_len as usize].to_vec()});
    links.quic.classifier=true;relay_gc(&mut links);0
}
unsafe extern "C" fn relay_get(user:*mut c_void,index:u32,out:*mut RelayRoute)->i32 {
    let (sh,me)=ctx(user);if out.is_null(){return ERR_ARG;}
    let links=sh.links.borrow();
    let route=links.quic.table.values().filter_map(|e| e.managed.as_ref())
        .filter(|(owner,_,_)|*owner==component_of(&me) as usize).nth(index as usize);
    if let Some((_,r,_))=route {*out=*r;0} else {ERR_NOTFOUND}
}
unsafe extern "C" fn relay_remove(user:*mut c_void,cid:*const u8,len:usize)->i32 {
    let (sh,me)=ctx(user);if cid.is_null() || len==0 || len>20 {return ERR_ARG;}
    let cid=std::slice::from_raw_parts(cid,len);let mut links=sh.links.borrow_mut();
    let Some(e)=links.quic.table.get(cid) else {return ERR_NOTFOUND;};
    if !e.managed.as_ref().is_some_and(|(owner,_,_)|*owner==component_of(&me) as usize) {return ERR_BUSY;}
    links.quic.table.remove(cid);relay_gc(&mut links);0
}
static QUIC_COMPONENT:Quic=Quic {version:QUIC_VERSION,size:std::mem::size_of::<Quic>() as u32,
    attach:None,detach:None,relay_set:Some(relay_set),relay_get:Some(relay_get),relay_remove:Some(relay_remove),
    relay_create:Some(relay_create),relay_stats:Some(relay_stats),relay_expire:Some(relay_expire),relay_idle:Some(relay_idle)};
pub(crate) fn quic_component()->*const Quic {&QUIC_COMPONENT}

fn relay_select(links:&mut Links,cid:&[u8],now:u64)->Option<quic::QuicEntry> {
    links.quic.table.get_mut(cid).filter(|e|e.managed.is_none() || e.idle_ms==0 || now.saturating_sub(e.last_used_us)<e.idle_ms as u64*1000).map(|entry| {
        if entry.managed.is_some(){entry.last_used_us=now;}
        if let Some((_,route,cursor))=&mut entry.managed {
            let total:u32=route.hops[..route.hop_count as usize].iter().map(|h|h.weight as u32).sum();
            let mut at=*cursor%total;*cursor=cursor.wrapping_add(1);
            for hop in &route.hops[..route.hop_count as usize] {
                if at<hop.weight as u32 {entry.egress=hop.link;entry.token=hop.token;
                    entry.out_cid=hop.rewritten_dcid[..hop.cid_len as usize].to_vec();
                    entry.address=hop.address[..hop.address_len as usize].to_vec();break;}
                at-=hop.weight as u32;
            }
        }
        entry.clone()
    })
}

fn relay_gc(links:&mut Links) {
    if !links.quic.table.values().any(|e|e.managed.as_ref().is_some_and(|(_,r,_)|r.endpoint!=0)) {
        if let Some(at)=links.find("relay") {
            let l=&mut links.list[at];l.provider=LINK_NONE;l.state=LINK_DOWN;
            if l.subs.is_empty(){*l=Entry::default();}
        }
    }
}

unsafe extern "C" fn relay_create(user:*mut c_void,out:*mut RelayRoute,idle_ms:u32,reuse:u32)->i32 {
    let (sh,me)=ctx(user);if out.is_null() || reuse>1 {return ERR_ARG;}
    let mut r=*out;if r.dcid_len!=0 || r.hop_count!=1 {return ERR_ARG;}
    relay_expire(user);let now=sh.st.borrow().now*1000;let owner=component_of(&me) as usize;
    if reuse!=0 {
        let mut links=sh.links.borrow_mut();
        for e in links.quic.table.values_mut(){
            if let Some((old_owner,old,_))=&e.managed {
                if *old_owner==owner && e.idle_ms==idle_ms && old.endpoint==r.endpoint && old.hop_count==1 && old.hops[0]==r.hops[0] {
                    *out=*old;e.last_used_us=now;return 1;
                }
            }
        }
    }
    for _ in 0..=quic::TABLE_MAX {
        let cid={let mut links=sh.links.borrow_mut();let state=&mut links.quic;
            if state.next_relay_cid>=(1u64<<60)-1 {return ERR_NOMEM;}
            state.next_relay_cid+=1;quic::cid_encode((1u64<<60)|state.next_relay_cid).unwrap()};
        if sh.links.borrow().quic.table.contains_key(&cid){continue;}
        r.dcid[..cid.len()].copy_from_slice(&cid);r.dcid_len=cid.len() as u8;
        let rc=relay_set(user,&r,0);if rc<0{return rc;}
        let mut links=sh.links.borrow_mut();let e=links.quic.table.get_mut(&cid).unwrap();e.last_used_us=now;e.idle_ms=idle_ms;*out=r;return 0;
    }
    ERR_NOMEM
}
unsafe extern "C" fn relay_stats(user:*mut c_void,cid:*const u8,len:usize,out:*mut RelayStats)->i32 {
    let (sh,me)=ctx(user);if cid.is_null() || len==0 || len>20 || out.is_null(){return ERR_ARG;}
    let links=sh.links.borrow();let Some(e)=links.quic.table.get(std::slice::from_raw_parts(cid,len)) else{return ERR_NOTFOUND;};
    if !e.managed.as_ref().is_some_and(|(owner,_,_)|*owner==component_of(&me) as usize){return ERR_NOTFOUND;}
    *out=RelayStats {last_used_us:e.last_used_us,idle_ms:e.idle_ms,reserved:0};0
}
unsafe extern "C" fn relay_expire(user:*mut c_void)->i32 {
    let (sh,me)=ctx(user);let now=sh.st.borrow().now*1000;let mut links=sh.links.borrow_mut();let before=links.quic.table.len();
    links.quic.table.retain(|_,e| !e.managed.as_ref().is_some_and(|(owner,_,_)|*owner==component_of(&me) as usize && e.idle_ms!=0 && now.saturating_sub(e.last_used_us)>=e.idle_ms as u64*1000));
    let removed=before-links.quic.table.len();relay_gc(&mut links);removed as i32
}
unsafe extern "C" fn relay_idle(user:*mut c_void,cid:*const u8,len:usize,idle_ms:u32)->i32 {
    let (sh,me)=ctx(user);if cid.is_null() || len==0 || len>20 {return ERR_ARG;}
    let mut links=sh.links.borrow_mut();let Some(e)=links.quic.table.get_mut(std::slice::from_raw_parts(cid,len)) else{return ERR_NOTFOUND;};
    if !e.managed.as_ref().is_some_and(|(owner,_,_)|*owner==component_of(&me) as usize){return ERR_NOTFOUND;}
    e.idle_ms=idle_ms;0
}
