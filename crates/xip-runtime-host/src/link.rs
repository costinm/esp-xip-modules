//! The `link` component of the host-runtime. The same rules as the device runtimes (`xip_runtime.c`): named links with a provider
//! (a native one the test plays, or a module) and subscribers; frames delivered as packets with `CALL_LINK`, or forwarded by a
//! route before anyone sees them; `send` to a link address, with the packet coming back as a `TX_DONE` event when the sender asked;
//! `READY` and `STATE` events. Up to 6 links, 6 subscribers and 16 routes each, and 16 sends waiting for completion.
use std::ffi::{c_char, c_int, c_void, CStr};
use std::rc::Rc;
use xip_abi::*;

use crate::hostcalls::ctx;
use crate::pool::HELD_OWNED;
use crate::runtime::{deliver, route, Inst, QItem, Runtime, Shared};

const LINKS_MAX: usize = 6;
const SUBS_MAX: usize = 6;
const ROUTES_MAX: usize = 16;
const PENDING_MAX: usize = 16;

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

struct Pending {
    addr: u32,
    slot: i32,
    start: u16,
    len: u16,
    sender: i32,
    component: i32,
    method: i32,
    token: u64,
    t0_ms: u64,
}

#[derive(Default)]
pub(crate) struct Links {
    list: Vec<Entry>,
    pending: Vec<Pending>,
}

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
        if self.list.len() >= LINKS_MAX {
            return None;
        }
        self.list.push(Entry { name: name.to_owned(), ..Default::default() });
        Some(self.list.len() - 1)
    }
    fn at(&mut self, addr: u32) -> Option<&mut Entry> {
        if addr < ADDR_LINK {
            return None;
        }
        self.list.get_mut((addr - ADDR_LINK) as usize)
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

/// TODO: the host-runtime should share the receive path of the Rust runtime (`xip-runtime-esp::uart_frames`: the PPP-style decoder that writes a record straight into a
/// pool packet, and the line editor for the console port) instead of binding by subscription and injecting whole frames, so the tests exercise the decoder, the
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

/// The provider is done with the frame in `slot`: the reference the runtime kept goes back to the sender (TX_DONE) or is released.
fn complete(sh: &Shared, slot: i32, outcome: i32) {
    let taken = {
        let mut links = sh.links.borrow_mut();
        links.pending.iter().position(|p| p.slot == slot).map(|at| links.pending.remove(at))
    };
    let Some(p) = taken else {
        sh.pool.release(slot);
        return;
    };
    let elapsed = (sh.st.borrow().now.saturating_sub(p.t0_ms) * 1000) as u32;
    let sent = route(sh, p.sender).is_some_and(|t| {
        deliver(sh, &t, slot, p.start, p.len, p.component, p.method, p.addr, p.token, outcome, CALL_LINK | CALL_LINK_EVENT | (LINK_EVENT_TX_DONE << 8), elapsed) == 0
    });
    if !sent {
        sh.pool.release(slot);
    }
}

/// A provider module's handler returned without keeping a frame it was given: sent.
pub(crate) fn tx_returned(sh: &Shared, slot: i32) {
    complete(sh, slot, 0);
}

/// Send the frame in `slot` (one reference is the caller's) to the link: on 0 the runtime owns the reference, otherwise the caller
/// keeps it. `component` / `method` (with `sender`): where the packet returns when done; both 0: nowhere.
fn link_tx(sh: &Shared, dst: u32, slot: i32, start: u16, len: u16, token: u64, sender: i32, component: i32, method: i32) -> i32 {
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
    let completion = component != 0 || method != 0;
    if completion {
        let mut links = sh.links.borrow_mut();
        if links.pending.len() >= PENDING_MAX {
            return ERR_BUSY;
        }
        let t0_ms = sh.st.borrow().now;
        links.pending.push(Pending { addr: dst, slot, start, len, sender, component, method, token, t0_ms });
    }
    let unpend = |sh: &Shared| {
        if completion {
            sh.links.borrow_mut().pending.retain(|p| p.slot != slot);
        }
    };
    let rc = match target {
        Target::Native(NativeTx::Busy) => {
            unpend(sh);
            ERR_BUSY
        }
        Target::Native(mode) => {
            let frame = unsafe { std::slice::from_raw_parts(sh.pool.slot(slot).add(start as usize), len as usize) }.to_vec();
            {
                let mut links = sh.links.borrow_mut();
                let l = links.at(dst).unwrap();
                l.native_tx.push((token, frame));
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
            sh.pool.retain(slot); // the provider's inbox gets its own reference
            let rc = match route(sh, owner) {
                Some(t) => deliver(sh, &t, slot, start, len, tx_component, tx_method, sender as u32, token, 0, CALL_LINK, 0),
                None => ERR_NOTFOUND,
            };
            if rc != 0 {
                sh.pool.release(slot);
                unpend(sh);
            } else if !completion {
                sh.pool.release(slot); // nothing to give back: the provider's reference is the last
            }
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

/// `send(dst = link address)`. `ERR_BUSY` is the one failure that leaves the packet with the sender.
pub(crate) fn send(sh: &Shared, me: &Rc<Inst>, pkt: u32, len: usize, dst: u32, component: i32, method: i32, token: u64) -> i32 {
    let held = me.st.borrow_mut().held.get(pkt);
    let Some(e) = held.filter(|e| e.state as u32 == HELD_OWNED) else {
        me.st.borrow_mut().held.take(pkt); // a borrowed or unknown handle: consumed, as for any send
        return ERR_ARG;
    };
    let slot = e.slot as i32;
    if len > PKT_MAX as usize {
        me.st.borrow_mut().held.take(pkt);
        sh.pool.release(slot);
        return ERR_SIZE;
    }
    let rc = link_tx(sh, dst, slot, e.start, len as u16, token, component_of(me), component, method);
    if rc == ERR_BUSY {
        return rc;
    }
    me.st.borrow_mut().held.take(pkt);
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
    let Some(e) = me.st.borrow_mut().held.take(pkt) else { return ERR_ARG };
    sh.pool.release(e.slot as i32); // the provider's own reference
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

/// The packet in `slot` (one reference is the caller's) goes to the route or to every subscriber: one reference each.
fn fanout(sh: &Shared, addr: u32, slot: i32, start: u16, len: u16, source: u64, signal: i32, time_us: u32) {
    if let Some(l) = sh.links.borrow_mut().at(addr) {
        l.received += 1;
    }
    if link_route(sh, addr, slot, start, len) {
        return;
    }
    let subs = sh.links.borrow_mut().at(addr).map(|l| l.subs.clone()).unwrap_or_default();
    if subs.is_empty() {
        sh.pool.release(slot);
        return;
    }
    let mut lost = 0;
    for (k, (owner, component, method)) in subs.iter().enumerate() {
        if k + 1 < subs.len() {
            sh.pool.retain(slot);
        }
        let ok = route(sh, *owner).is_some_and(|t| deliver(sh, &t, slot, start, len, *component, *method, addr, source, signal, CALL_LINK, time_us) == 0);
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
    let Some(e) = me.st.borrow_mut().held.take(pkt) else { return ERR_ARG };
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
        i.subscribers = self.subs.len() as u32;
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
        loop {
            let slot = sh.links.borrow().pending.iter().find(|p| p.addr == addr).map(|p| p.slot);
            let Some(slot) = slot else { break };
            complete(sh, slot, ERR_IO);
        }
        event(sh, addr, LINK_EVENT_STATE, LINK_DOWN as i32, 0);
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

    /// A frame arrives on the native link `name`: copied into a pool packet, stamped now, routed or delivered to the subscribers
    /// (nothing happens when nobody listens). Returns the usual result.
    pub fn inject_frame(&self, name: &str, source: u64, signal: i32, data: &[u8]) -> i32 {
        let sh = &*self.sh;
        let Some(at) = sh.links.borrow().find(name) else { return ERR_NOTFOUND };
        let addr = addr_of(at);
        if data.len() > PKT_MAX as usize {
            return ERR_SIZE;
        }
        if sh.links.borrow_mut().at(addr).is_some_and(|l| l.subs.is_empty() && l.routes.is_empty()) {
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
