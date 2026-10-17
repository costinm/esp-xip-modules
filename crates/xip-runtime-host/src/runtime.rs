//! The runtime: instances, the event loop with virtual time, alarms, pins and the application side.
use crate::fixtures::SpiDevice;
use crate::pool::{Held, Mem, Pool, HELD_BORROWED, HELD_FREE, HELD_OWNED};
use crate::{hostcalls, HandlerFn, Loaded};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::rc::Rc;
use xip_abi::*;

pub(crate) const PINS: usize = 49;
pub(crate) const ALARMS: usize = 8;
pub(crate) const QUEUE: usize = 64;
pub(crate) const INBOX_DATA: usize = 1024;

/// Limits of the runtime. The defaults are the small numbers of the C simulator, so that the limit cases are easy to hit.
#[derive(Clone, Debug)]
pub struct Config {
    /// Slots of the shared packet pool.
    pub pool_slots: u16,
    /// Packets one module may own at a time.
    pub held_quota: u8,
    /// Packets waiting for one module before senders get `ERR_BUSY`.
    pub inbox: usize,
    /// Cap of `mem_alloc` bytes per module (0: none).
    pub mem_quota: u32,
}
impl Default for Config {
    fn default() -> Self {
        Config { pool_slots: 8, held_quota: 4, inbox: 4, mem_quota: 8192 }
    }
}

/// One `emit` of a module.
#[derive(Clone, Debug)]
pub struct Event {
    pub id: u16,
    pub kind: u8,
    pub data: Vec<u8>,
}
impl Event {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// A packet a module sent to the application (`ADDR_MAIN`).
#[derive(Clone, Debug)]
pub struct MainMsg {
    pub from: u32,
    pub component: i32,
    pub method: i32,
    pub id: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceKind {
    /// The handler was called: component, method, what it returned.
    Handler { component: i32, method: i32, result: i32 },
    /// The module called a host function.
    Host(&'static str),
}
/// One line of the trace (see [`Runtime::set_trace`]).
#[derive(Clone, Debug)]
pub struct Trace {
    pub t_ms: u64,
    pub instance: usize,
    pub kind: TraceKind,
}

#[derive(Default, Clone)]
pub(crate) struct Pin {
    pub owner: Option<usize>,
    pub level: bool,
    pub wire: Option<usize>,
    pub irq: Option<Irq>,
}
#[derive(Clone)]
pub(crate) struct Irq {
    pub edge: i32,
    pub component: i32,
    pub method: i32,
    pub masked: bool,
}

/// Everything the runtime shares between instances, and the state the console-style calls report. Fields are public so a
/// test can set up what `modules_info` and friends answer and read what the module asked for.
pub struct RtState {
    pub now: u64,
    pub connected: bool,
    pub(crate) window_first: u64,
    pub(crate) window_period: u64,
    /// Packets modules sent to the application.
    pub main: Vec<MainMsg>,
    /// What modules wrote with `serial_write`.
    pub serial_out: Vec<u8>,
    pub serial_component: i32,
    pub serial_method: i32,
    pub reset_called: bool,
    pub started: String,
    pub start_request: String,
    pub stopped: String,
    /// What `modules_info`, `events_info` and `module_manifest("test")` return.
    pub modules_cbor: Vec<u8>,
    pub events_cbor: Vec<u8>,
    pub manifest: Vec<u8>,
    pub(crate) nv_value: String,
    pub(crate) pins: Vec<Pin>,
    pub(crate) spi: HashMap<i32, Box<dyn SpiDevice>>,
    pub(crate) components: HashMap<String, usize>,
    pub(crate) trace_on: bool,
    pub(crate) trace: Vec<Trace>,
}

#[derive(Clone, Default)]
pub(crate) struct Alarm {
    pub used: bool,
    pub waiting_net: bool,
    pub due: u64,
    pub fire: u64,
    pub interval: u32,
    pub window: u32,
    pub flags: u32,
    pub component: i32,
    pub method: i32,
}

#[derive(Default)]
pub(crate) struct QItem {
    pub component: i32,
    pub method: i32,
    pub value: i32,
    /// 0 event, 1 request without answer, 2 request with answer, 3 packet
    pub request: u8,
    pub pslot: i32,
    pub pstart: u16,
    pub plen: u16,
    pub from: u32,
    pub id: u64,
    /// `CALL_LINK*` flags and kind (frames and events of a link), and the time stamp of the call.
    pub lflags: u32,
    pub time_us: u32,
    pub data: Vec<u8>,
}

pub(crate) struct InstState {
    pub queue: VecDeque<QItem>,
    pub alarms: Vec<Alarm>,
    pub events: Vec<Event>,
    pub logs: Vec<String>,
    pub settings: HashMap<Vec<u8>, Vec<u8>>,
    pub spi_open: [Option<i32>; 4],
    pub spi_transfers: u32,
    pub held: Box<Held>,
    pub mem: Box<Mem>,
    pub response: Option<Vec<u8>>,
    pub in_request: bool,
    pub responded: bool,
    pub component: Option<i32>,
    pub ended: bool,
    pub result: i32,
    pub calls: u32,
    pub leaked: u32,
    pub mem_leaked: u32,
}

pub(crate) struct Ctx {
    pub sh: *const Shared,
    pub id: usize,
}

pub(crate) struct Inst {
    pub id: usize,
    pub name: String,
    pub _ctx: Box<Ctx>,
    pub table: Box<Host>,
    pub handler: HandlerFn,
    pub state_ptr: *mut u8,
    _state: Box<[u128]>,
    _loaded: Option<Loaded>,
    pub st: RefCell<InstState>,
}

pub(crate) struct Shared {
    pub cfg: Config,
    pub pool: Pool,
    pub st: RefCell<RtState>,
    pub insts: RefCell<Vec<Rc<Inst>>>,
    pub links: RefCell<crate::link::Links>,
}

/// The runtime. Single threaded and deterministic: the application calls [`Runtime::run`] and the modules' handlers run
/// inside it, one call at a time.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) sh: Rc<Shared>,
}

/// A running (or ended) module instance.
#[derive(Clone)]
pub struct Instance {
    rt: Runtime,
    me: Rc<Inst>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(Config::default())
    }
}

impl Runtime {
    pub fn new(cfg: Config) -> Runtime {
        let st = RtState {
            now: 0,
            connected: false,
            window_first: 0,
            window_period: 0,
            main: Vec::new(),
            serial_out: Vec::new(),
            serial_component: -1,
            serial_method: -1,
            reset_called: false,
            started: String::new(),
            start_request: String::new(),
            stopped: String::new(),
            modules_cbor: Vec::new(),
            events_cbor: Vec::new(),
            manifest: Vec::new(),
            nv_value: String::new(),
            pins: vec![Pin::default(); PINS],
            spi: HashMap::new(),
            components: HashMap::new(),
            trace_on: false,
            trace: Vec::new(),
        };
        crate::wifi::FIX.with(|f| *f.borrow_mut() = crate::wifi::Fixture::new());
        crate::wifi::TUNING.with(|t| *t.borrow_mut() = xip_abi::WifiTuning::default());
        crate::power::FIX.with(|f| f.borrow_mut().requests.clear());
        let sh = Rc::new(Shared { pool: Pool::new(cfg.pool_slots), cfg, st: RefCell::new(st), insts: RefCell::new(Vec::new()), links: RefCell::new(Default::default()) });
        Runtime { sh }
    }

    /// Create an instance for a module loaded from a shared object. `state_bytes` is the size the image header would ask for.
    pub fn add_module(&self, path: impl AsRef<std::path::Path>, state_bytes: usize) -> Result<Instance, String> {
        let loaded = Loaded::open(path.as_ref())?;
        let name = path.as_ref().file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(self.add_loaded(&name, loaded, state_bytes))
    }

    /// Create an instance around a handler linked into this process.
    pub fn add_handler(&self, name: &str, handler: HandlerFn, state_bytes: usize) -> Instance {
        self.add_loaded(name, Loaded::linked(handler), state_bytes)
    }

    fn add_loaded(&self, name: &str, loaded: Loaded, state_bytes: usize) -> Instance {
        let sh = &self.sh;
        let id = sh.insts.borrow().len();
        let mut state: Box<[u128]> = vec![0u128; state_bytes.max(1).div_ceil(16)].into_boxed_slice();
        let state_ptr = state.as_mut_ptr().cast::<u8>();
        let ctx = Box::new(Ctx { sh: Rc::as_ptr(sh), id });
        let mut table: Box<Host> = Box::new(unsafe { std::mem::zeroed() });
        hostcalls::fill(&mut table, &*ctx as *const Ctx as *mut c_void);
        let inst = Rc::new(Inst {
            id,
            name: name.to_string(),
            _ctx: ctx,
            table,
            handler: loaded.handler,
            state_ptr,
            _state: state,
            _loaded: Some(loaded),
            st: RefCell::new(InstState {
                queue: VecDeque::new(),
                alarms: vec![Alarm::default(); ALARMS],
                events: Vec::new(),
                logs: Vec::new(),
                settings: HashMap::new(),
                spi_open: [None; 4],
                spi_transfers: 0,
                held: Held::new(sh.cfg.held_quota),
                mem: Mem::new(sh.cfg.mem_quota),
                response: None,
                in_request: false,
                responded: false,
                component: None,
                ended: false,
                result: 0,
                calls: 0,
                leaked: 0,
                mem_leaked: 0,
            }),
        });
        sh.insts.borrow_mut().push(inst.clone());
        Instance { rt: self.clone(), me: inst }
    }

    /// Shared state of the runtime (virtual time, what modules wrote to the application, console fixtures).
    pub fn state(&self) -> Ref<'_, RtState> {
        self.sh.st.borrow()
    }
    pub fn state_mut(&self) -> RefMut<'_, RtState> {
        self.sh.st.borrow_mut()
    }

    pub fn now(&self) -> u64 {
        self.sh.st.borrow().now
    }

    /// Packets available in the shared pool.
    pub fn pool_available(&self) -> u16 {
        self.sh.pool.available()
    }

    /// Make `table` what `get_component(name)` returns (for example the `psa` table of a software provider). The table must
    /// stay valid as long as the runtime lives.
    pub fn register_component(&self, name: &str, table: *const c_void) {
        self.sh.st.borrow_mut().components.insert(name.to_string(), table as usize);
    }

    /// Attach a device to the SPI chip-select `cs` (the default is a loopback).
    pub fn set_spi_device(&self, cs: i32, dev: Box<dyn SpiDevice>) {
        self.sh.st.borrow_mut().spi.insert(cs, dev);
    }

    /// Record the handler calls and host calls of every instance (see [`Runtime::take_trace`]).
    pub fn set_trace(&self, on: bool) {
        self.sh.st.borrow_mut().trace_on = on;
    }
    pub fn take_trace(&self) -> Vec<Trace> {
        std::mem::take(&mut self.sh.st.borrow_mut().trace)
    }

    /// Sleepy mode: wake windows start `first_ms` from now and repeat every `period_ms` (0: none).
    pub fn set_window(&self, first_ms: u32, period_ms: u32) {
        let mut s = self.sh.st.borrow_mut();
        s.window_first = s.now + first_ms as u64;
        s.window_period = period_ms as u64;
    }

    /// Main's network path changes: HOST_NETWORK to every instance, and `ALARM_CONNECTED` alarms become due.
    pub fn set_connected(&self, connected: bool) {
        {
            let mut s = self.sh.st.borrow_mut();
            if s.connected == connected {
                return;
            }
            s.connected = connected;
        }
        let now = self.now();
        for me in self.sh.insts.borrow().iter() {
            enqueue(me, HOST_COMPONENT as i32, HOST_NETWORK as i32, connected as i32);
            if connected {
                let mut st = me.st.borrow_mut();
                for a in st.alarms.iter_mut().filter(|a| a.used && a.waiting_net) {
                    a.waiting_net = false;
                    a.due = now;
                    a.fire = now + fire_delay(&self.sh.st.borrow(), 0, a.window, a.flags) as u64;
                }
            }
        }
    }

    /// The wanted Wi-Fi state modules last set through the `wifi` component, and its generation.
    pub fn wifi_intent(&self) -> (xip_abi::WifiIntent, u32) {
        crate::wifi::FIX.with(|f| { let f = f.borrow(); (f.intent, f.generation) })
    }

    /// The settings modules last set through the `wifi-tuning` component.
    /// The sleep requests the `power` fixture recorded, and the last `wifi.monitor` request.
    pub fn power_requests(&self) -> Vec<xip_abi::PowerSleep> {
        crate::power::FIX.with(|f| f.borrow().requests.clone())
    }
    pub fn wifi_monitor(&self) -> Option<xip_abi::WifiMonitor> {
        crate::wifi::FIX.with(|f| f.borrow().monitor)
    }
    pub fn wifi_tuning(&self) -> xip_abi::WifiTuning {
        crate::wifi::TUNING.with(|t| *t.borrow())
    }

    /// Script what `wifi.state` and `wifi_info` report, instead of the default (a station intent joins at once).
    pub fn set_wifi_state(&self, state: xip_abi::WifiState) {
        crate::wifi::FIX.with(|f| { let mut f = f.borrow_mut(); f.state = state; f.scripted = true; });
    }

    /// An output on `from_pin` drives `to_pin`.
    pub fn wire(&self, from_pin: usize, to_pin: usize) {
        self.sh.st.borrow_mut().pins[from_pin].wire = Some(to_pin);
    }

    /// The outside world changes a pin.
    pub fn drive(&self, pin: usize, level: bool) {
        drive(&self.sh, pin, level);
    }

    /// Run the instances for `ms` of virtual time: queued calls first, then alarms in time order.
    pub fn run(&self, ms: u32) {
        let sh = &*self.sh;
        let end = sh.st.borrow().now + ms as u64;
        loop {
            drain_all(sh);
            let mut next: Option<(u64, Rc<Inst>, usize)> = None;
            for me in sh.insts.borrow().iter() {
                let st = me.st.borrow();
                if st.ended {
                    continue;
                }
                for (i, a) in st.alarms.iter().enumerate() {
                    if a.used && !a.waiting_net && next.as_ref().map_or(true, |n| a.fire < n.0) {
                        next = Some((a.fire, me.clone(), i));
                    }
                }
            }
            let Some((fire, me, i)) = next else { break };
            if fire > end {
                break;
            }
            {
                let mut s = sh.st.borrow_mut();
                if fire > s.now {
                    s.now = fire;
                }
            }
            fire_alarm(sh, &me, i);
        }
        sh.st.borrow_mut().now = end;
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let insts: Vec<Rc<Inst>> = self.insts.get_mut().clone();
        for me in insts.iter() {
            release_all(self, me);
        }
    }
}

pub(crate) fn enqueue(me: &Inst, component: i32, method: i32, value: i32) {
    let mut st = me.st.borrow_mut();
    if st.queue.len() < QUEUE {
        st.queue.push_back(QItem { component, method, value, ..Default::default() });
    }
}

fn window_ms(s: &RtState) -> u32 {
    if s.window_period == 0 {
        return 0;
    }
    if s.now < s.window_first {
        return (s.window_first - s.now) as u32;
    }
    let into = (s.now - s.window_first) % s.window_period;
    (s.window_period - into) as u32
}

/// The alarm rules of the device runtime: a flexible alarm fires in the next wake window inside `[delay, delay + window]`.
pub(crate) fn fire_delay(s: &RtState, delay: u32, window: u32, flags: u32) -> u32 {
    let w = window_ms(s);
    if flags & ALARM_FLEXIBLE == 0 || w == 0 {
        return delay;
    }
    if w >= delay && w - delay <= window {
        return w;
    }
    if w < delay {
        delay
    } else {
        delay.saturating_add(window)
    }
}

pub(crate) fn drive(sh: &Shared, pin: usize, level: bool) {
    let target = {
        let mut s = sh.st.borrow_mut();
        let p = &mut s.pins[pin];
        if p.level == level {
            return;
        }
        p.level = level;
        match &p.irq {
            Some(i) if !i.masked && (i.edge == IRQ_BOTH as i32 || (i.edge == IRQ_RISING as i32 && level) || (i.edge == IRQ_FALLING as i32 && !level)) => {
                p.owner.map(|o| (o, i.component, i.method))
            }
            _ => None,
        }
    };
    if let Some((owner, component, method)) = target {
        let me = sh.insts.borrow()[owner].clone();
        enqueue(&me, component, method, level as i32);
    }
}

pub(crate) fn trace_host(sh: &Shared, me: &Inst, name: &'static str) {
    let mut s = sh.st.borrow_mut();
    if s.trace_on {
        let t_ms = s.now;
        s.trace.push(Trace { t_ms, instance: me.id, kind: TraceKind::Host(name) });
    }
}

/// Call the handler once.
fn dispatch(sh: &Shared, me: &Rc<Inst>, component: i32, method: i32, value: i32, flags: u32, data: &[u8]) -> i32 {
    if me.st.borrow().ended {
        return 0;
    }
    let call = Call {
        size: std::mem::size_of::<Call>() as u32,
        component,
        method,
        value,
        flags,
        data: data.as_ptr(),
        len: data.len(),
        pkt: 0,
        from: 0,
        time_us: 0,
        id: 0,
    };
    me.st.borrow_mut().calls += 1;
    let rc = unsafe { (me.handler)(&*me.table, me.state_ptr, &call) };
    finish_call(sh, me, component, method, rc);
    rc
}

fn finish_call(sh: &Shared, me: &Rc<Inst>, component: i32, method: i32, rc: i32) {
    let mut st = me.st.borrow_mut();
    st.result = rc;
    if rc < 0 || rc == RET_DONE as i32 || (component == HOST_COMPONENT as i32 && method == HOST_STOP as i32) {
        st.ended = true;
    }
    drop(st);
    let mut s = sh.st.borrow_mut();
    if s.trace_on {
        let t_ms = s.now;
        s.trace.push(Trace { t_ms, instance: me.id, kind: TraceKind::Handler { component, method, result: rc } });
    }
}

/// A module that ended gives back what it still holds.
fn release_all(sh: &Shared, me: &Rc<Inst>) {
    if let Some(component) = me.st.borrow().component {
        crate::link::release(sh, component);
    }
    let mut st = me.st.borrow_mut();
    st.mem_leaked = st.mem.used;
    for i in 0..st.held.e.len() {
        let e = st.held.e[i];
        if e.state == HELD_FREE {
            continue;
        }
        if e.state as u32 == HELD_OWNED {
            st.leaked += 1;
        }
        sh.pool.release(e.slot as i32);
        st.held.e[i].state = HELD_FREE;
    }
    for q in st.queue.iter_mut().filter(|q| q.request == 3) {
        sh.pool.release(q.pslot);
        q.request = 0;
        q.component = -1;
    }
}

fn run_call(sh: &Shared, me: &Rc<Inst>, c: QItem) {
    if c.request == 3 {
        let handle = me.st.borrow_mut().held.add(c.pslot, c.pstart, c.plen, HELD_BORROWED);
        if handle == 0 {
            sh.pool.release(c.pslot);
            return;
        }
        let wants = c.id != 0 && c.lflags & CALL_LINK == 0;
        {
            let mut st = me.st.borrow_mut();
            st.in_request = wants;
            st.responded = false;
            st.response = None;
            st.calls += 1;
        }
        let call = Call {
            size: std::mem::size_of::<Call>() as u32,
            component: c.component,
            method: c.method,
            value: c.value,
            flags: wants as u32 | c.lflags,
            data: unsafe { sh.pool.slot(c.pslot).add(c.pstart as usize) },
            len: c.plen as usize,
            pkt: handle,
            from: c.from,
            time_us: c.time_us,
            id: c.id,
        };
        let rc = unsafe { (me.handler)(&*me.table, me.state_ptr, &call) };
        finish_call(sh, me, c.component, c.method, rc);
        let answer = {
            let mut st = me.st.borrow_mut();
            st.in_request = false;
            if wants && st.responded { st.response.take() } else { None }
        };
        if let Some(data) = answer {
            send_answer(sh, me, c.from, c.component, c.method, c.id, &data);
        }
        let mut st = me.st.borrow_mut();
        // a packet still borrowed (not kept, not sent) goes back to the pool
        if matches!(st.held.get(handle), Some(e) if e.state as u32 == HELD_BORROWED) {
            st.held.take(handle);
            sh.pool.release(c.pslot);
            drop(st);
            // a frame handed to a provider that did not keep it is sent, as far as the sender is concerned
            if c.lflags & CALL_LINK != 0 && c.lflags & CALL_LINK_EVENT == 0 && c.from < ADDR_LINK {
                crate::link::tx_returned(sh, c.pslot);
            }
        }
        return;
    }
    if c.request == 4 {
        // an event of a link: no packet
        let call = Call {
            size: std::mem::size_of::<Call>() as u32,
            component: c.component,
            method: c.method,
            value: c.value,
            flags: c.lflags,
            data: std::ptr::null(),
            len: 0,
            pkt: 0,
            from: c.from,
            time_us: 0,
            id: c.id,
        };
        me.st.borrow_mut().calls += 1;
        let rc = unsafe { (me.handler)(&*me.table, me.state_ptr, &call) };
        finish_call(sh, me, c.component, c.method, rc);
        return;
    }
    if c.request != 0 {
        {
            let mut st = me.st.borrow_mut();
            st.in_request = true;
            st.responded = false;
        }
        dispatch(sh, me, c.component, c.method, 0, (c.request == 2) as u32, &c.data);
        me.st.borrow_mut().in_request = false;
    } else {
        dispatch(sh, me, c.component, c.method, c.value, 0, &c.data);
    }
}

fn drain(sh: &Shared, me: &Rc<Inst>) -> bool {
    let mut any = false;
    loop {
        let item = {
            let mut st = me.st.borrow_mut();
            if st.ended {
                None
            } else {
                st.queue.pop_front()
            }
        };
        let Some(item) = item else { break };
        any = true;
        run_call(sh, me, item);
        if me.st.borrow().ended {
            release_all(sh, me);
        }
    }
    if me.st.borrow().ended {
        release_all(sh, me);
        me.st.borrow_mut().queue.clear();
    }
    any
}

/// A new wanted Wi-Fi state: `HOST_WIFI` (its generation) to every instance, as the device runtime does after reconciling.
fn announce_wifi(sh: &Shared) {
    let (pending, generation) = crate::wifi::FIX.with(|f| {
        let mut f = f.borrow_mut();
        (std::mem::replace(&mut f.pending, false), f.generation)
    });
    if pending {
        for me in sh.insts.borrow().iter() {
            enqueue(me, HOST_COMPONENT as i32, HOST_WIFI as i32, generation as i32);
        }
    }
}

/// Round robin over the instances until nobody has a queued call.
fn drain_all(sh: &Shared) {
    loop {
        announce_wifi(sh);
        let insts: Vec<Rc<Inst>> = sh.insts.borrow().clone();
        let mut any = false;
        for me in &insts {
            any |= drain(sh, me);
        }
        if !any {
            break;
        }
    }
}

fn fire_alarm(sh: &Shared, me: &Rc<Inst>, i: usize) {
    let (now, connected) = {
        let s = sh.st.borrow();
        (s.now, s.connected)
    };
    let a = me.st.borrow().alarms[i].clone();
    if a.flags & ALARM_CONNECTED != 0 && !connected {
        let mut st = me.st.borrow_mut();
        st.alarms[i].waiting_net = true;
        st.alarms[i].fire = u64::MAX;
        return;
    }
    let late = now.saturating_sub(a.due);
    dispatch(sh, me, a.component, a.method, late.min(0x7fff_ffff) as i32, 0, &[]);
    let mut st = me.st.borrow_mut();
    if st.ended || !st.alarms[i].used {
        return;
    }
    if a.interval == 0 {
        st.alarms[i].used = false;
        return;
    }
    let due = a.due + a.interval as u64;
    let left = due.saturating_sub(now) as u32;
    st.alarms[i].due = due;
    st.alarms[i].fire = now + fire_delay(&sh.st.borrow(), left, a.window, a.flags) as u64;
}

/// `respond()` on a packet that carried an id: the same bytes go to the sender as a message with that id.
fn send_answer(sh: &Shared, me: &Inst, to: u32, component: i32, method: i32, id: u64, data: &[u8]) {
    let from = me.st.borrow().component.unwrap_or(0);
    if to == ADDR_MAIN {
        sh.st.borrow_mut().main.push(MainMsg { from: from as u32, component, method, id, data: data.to_vec() });
        return;
    }
    let Some(t) = route(sh, to as i32) else { return };
    let Some(slot) = sh.pool.acquire(0) else { return };
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), sh.pool.slot(slot).add(PKT_HEADROOM as usize), data.len()) };
    if deliver(sh, &t, slot, PKT_HEADROOM as u16, data.len() as u16, component, method, from as u32, id, 0, 0, 0) != 0 {
        sh.pool.release(slot);
    }
}

pub(crate) fn route(sh: &Shared, component: i32) -> Option<Rc<Inst>> {
    sh.insts.borrow().iter().find(|i| {
        let st = i.st.borrow();
        st.component == Some(component) && !st.ended
    }).cloned()
}

/// Queue a pool packet for `t`. The caller keeps the slot reference when this fails.
pub(crate) fn deliver(sh: &Shared, t: &Rc<Inst>, slot: i32, start: u16, len: u16, component: i32, method: i32, from: u32, id: u64, value: i32, lflags: u32, time_us: u32) -> i32 {
    let mut st = t.st.borrow_mut();
    if st.ended {
        return ERR_NOTFOUND;
    }
    let waiting = st.queue.iter().filter(|q| q.request == 3).count();
    if waiting >= sh.cfg.inbox || st.queue.len() >= QUEUE {
        return ERR_BUSY;
    }
    st.queue.push_back(QItem { component, method, request: 3, pslot: slot, pstart: start, plen: len, from, id, value, lflags, time_us, ..Default::default() });
    0
}

impl Instance {
    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }
    pub fn name(&self) -> &str {
        &self.me.name
    }
    /// The address of this module for `send(dst = component)`.
    pub fn set_component(&self, component: i32) {
        self.me.st.borrow_mut().component = Some(component);
    }

    /// HOST_START with the start request.
    pub fn start(&self, request: &[u8]) {
        let mut st = self.me.st.borrow_mut();
        if st.queue.len() < QUEUE {
            st.queue.push_back(QItem {
                component: HOST_COMPONENT as i32,
                method: HOST_START as i32,
                data: request[..request.len().min(INBOX_DATA)].to_vec(),
                ..Default::default()
            });
        }
    }
    /// HOST_STOP.
    pub fn stop(&self) {
        enqueue(&self.me, HOST_COMPONENT as i32, HOST_STOP as i32, 0);
    }
    /// A request, no answer wanted. Delivered by the next [`Instance::run`].
    pub fn post(&self, component: i32, method: i32, data: &[u8]) -> i32 {
        if data.len() > INBOX_DATA {
            return ERR_SIZE;
        }
        let mut st = self.me.st.borrow_mut();
        if st.queue.len() >= QUEUE {
            return ERR_BUSY;
        }
        st.queue.push_back(QItem { component, method, request: 1, data: data.to_vec(), ..Default::default() });
        0
    }
    /// A request with an answer; runs the simulation until it is handled. `Err` is an ABI error code.
    pub fn request(&self, component: i32, method: i32, data: &[u8]) -> Result<Vec<u8>, i32> {
        let rc = self.post(component, method, data);
        if rc != 0 {
            return Err(rc);
        }
        self.me.st.borrow_mut().queue.back_mut().unwrap().request = 2;
        {
            let mut st = self.me.st.borrow_mut();
            st.responded = false;
            st.response = None;
        }
        self.rt.run(0);
        if !self.me.st.borrow().queue.is_empty() {
            return Err(ERR_BUSY);
        }
        let mut st = self.me.st.borrow_mut();
        Ok(if st.responded { st.response.take().unwrap_or_default() } else { Vec::new() })
    }
    /// A packet from the application (`from` = ADDR_MAIN) to this module.
    pub fn send(&self, component: i32, method: i32, id: u64, data: &[u8]) -> i32 {
        self.send_from(ADDR_MAIN, component, method, id, data)
    }
    /// A packet as if module `from` sent it.
    pub fn send_from(&self, from: u32, component: i32, method: i32, id: u64, data: &[u8]) -> i32 {
        let sh = &*self.rt.sh;
        if data.len() > PKT_MAX as usize {
            return ERR_SIZE;
        }
        let Some(slot) = sh.pool.acquire(0) else { return ERR_NOMEM };
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), sh.pool.slot(slot).add(PKT_HEADROOM as usize), data.len()) };
        let rc = deliver(sh, &self.me, slot, PKT_HEADROOM as u16, data.len() as u16, component, method, from, id, 0, 0, 0);
        if rc != 0 {
            sh.pool.release(slot);
        }
        rc
    }
    /// Run the whole runtime (all instances) for `ms`.
    pub fn run(&self, ms: u32) {
        self.rt.run(ms);
    }
    pub fn ended(&self) -> bool {
        self.me.st.borrow().ended
    }
    /// What the last handler call returned.
    pub fn result(&self) -> i32 {
        self.me.st.borrow().result
    }
    /// Handler calls so far.
    pub fn calls(&self) -> u32 {
        self.me.st.borrow().calls
    }
    pub fn events(&self) -> Vec<Event> {
        self.me.st.borrow().events.clone()
    }
    /// Index of the first event with `id` whose text contains `needle`.
    pub fn find_event(&self, id: u16, needle: &str) -> Option<usize> {
        self.me.st.borrow().events.iter().position(|e| e.id == id && String::from_utf8_lossy(&e.data).contains(needle))
    }
    pub fn logs(&self) -> Vec<String> {
        self.me.st.borrow().logs.clone()
    }
    pub fn active_alarms(&self) -> usize {
        self.me.st.borrow().alarms.iter().filter(|a| a.used).count()
    }
    pub fn spi_transfers(&self) -> u32 {
        self.me.st.borrow().spi_transfers
    }
    /// Packets this module owns.
    pub fn held_count(&self) -> u32 {
        self.me.st.borrow().held.count(HELD_OWNED)
    }
    /// Packets it still owned when it ended.
    pub fn leaked(&self) -> u32 {
        self.me.st.borrow().leaked
    }
    /// Bytes of `mem_alloc` not given back when it ended.
    pub fn mem_leaked(&self) -> u32 {
        self.me.st.borrow().mem_leaked
    }
}
