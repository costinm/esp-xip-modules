//! Cooperative async helpers without a new host ABI.
//!
//! One task is polled once per handler call. Calls become owned snapshots; no call
//! slice or host table is retained. Host events resume the task; an in-poll
//! self-wake schedules one further poll. Foreign futures requiring wakeups outside
//! a handler are not supported until the runtime provides a wake bridge.
//! Wakers and TaskContext must not escape into external threads or host tasks:
//! their allocation and final destruction require this module's handler allocator
//! context. Reference counting stabilizes the waker state; it does not make the executor or
//! the module allocator safe for concurrent use. This cannot run arbitrary Tokio
//! futures inside a module. A future wake bridge must handle both scheduling and
//! allocator/instance lifetime.
//! This initial driver is single-threaded, including future destruction. It is not
//! a preemptive executor: each poll must still respect the handler time budget.
//!
//! Reserve component `-2147483647` for this helper's alarms. Each live sleep
//! uses its own method, so an early notification re-arms only its actual timer.
//! Four incoming calls may wait while the future is busy. Further calls, failed
//! payload allocations, and packet-quota refusals increment `rejected_calls()`
//! and leave the task running. Rejected requests receive CBOR integer ERR_BUSY
//! with their original correlation/routing, reusing the borrowed packet where
//! possible so a full held quota can still reply. `failed_replies()` counts
//! delivery failures. Packet calls retain their original pool lease;
//! packetless calls reserve owned payload storage fallibly.
//!
//! Store `Task::default()` in zero-valid module state; on HOST_START call
//! `task.start(hw, |ctx| async move { ... })`, then route all subsequent calls,
//! including HOST_STOP, through `task.handle(hw, call)`. Inside the future use
//! `ctx.next_call().await`, `ctx.sleep(ms).await`, and synchronous host operations
//! through `ctx.with_hw(|hw| ...)`. Outside a poll `with_hw` returns ERR_ARG.
//! Return RET_DONE to end the module. Task startup and sleep registration return
//! ERR_NOMEM when the configured module heap cannot hold their allocations.
//!
//! An in-poll `wake_by_ref()` schedules one handler at least one millisecond later;
//! repeated self-wakes cost one handler per millisecond. External-thread/outside-
//! poll wakes need a future runtime wake bridge and trigger a debug assertion.
extern crate alloc;
use crate::{
    CallView, Hw, ADDR_LINK, ALARM_EXACT, ERR_ARG, ERR_BUSY, ERR_NOMEM, ERR_SIZE, HOST_STOP,
    RET_CONTINUE, RET_DONE,
};
use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use core::{
    alloc::Layout,
    cell::{Cell, RefCell},
    future::Future,
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

// Reserved by the module using this helper; do not register these coordinates elsewhere.
const COMPONENT: i32 = -2147483647;
const METHOD: i32 = 1;
const INBOX: usize = 4;

// Stable Rust's Box/Rc/Arc constructors abort on OOM. Keep the helper's few
// shared allocations fallible without imposing allocator_api on its callers.
fn try_box<T>(value: T) -> Result<Box<T>, i32> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        return Ok(Box::new(value));
    }
    let p = unsafe { alloc::alloc::alloc(layout).cast::<T>() };
    let Some(p) = NonNull::new(p) else {
        return Err(ERR_NOMEM);
    };
    unsafe {
        p.as_ptr().write(value);
        Ok(Box::from_raw(p.as_ptr()))
    }
}
struct RefInner<T> {
    refs: AtomicUsize,
    value: T,
}
struct SharedRef<T>(NonNull<RefInner<T>>);
impl<T> SharedRef<T> {
    fn try_new(value: T) -> Result<Self, i32> {
        Ok(Self(NonNull::from(Box::leak(try_box(RefInner {
            refs: AtomicUsize::new(1),
            value,
        })?))))
    }
    fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.0 == b.0
    }
    fn into_raw(self) -> *mut RefInner<T> {
        let p = self.0.as_ptr();
        core::mem::forget(self);
        p
    }
    unsafe fn from_raw(p: *mut RefInner<T>) -> Self {
        Self(NonNull::new_unchecked(p))
    }
    unsafe fn increment_strong_count(p: *mut RefInner<T>) {
        // Forgotten clones consume no additional memory. Refuse the increment
        // before overflow even if panic is caught repeatedly by a host caller.
        let incremented = (*p)
            .refs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |refs| {
                (refs < isize::MAX as usize).then(|| refs + 1)
            });
        assert!(incremented.is_ok(), "task reference count overflow");
    }
}
impl<T> Clone for SharedRef<T> {
    fn clone(&self) -> Self {
        unsafe {
            Self::increment_strong_count(self.0.as_ptr());
            Self(self.0)
        }
    }
}
impl<T> Deref for SharedRef<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &self.0.as_ref().value }
    }
}
impl<T> Drop for SharedRef<T> {
    fn drop(&mut self) {
        if unsafe { self.0.as_ref() }
            .refs
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            unsafe {
                drop(Box::from_raw(self.0.as_ptr()));
            }
        }
    }
}
type Rc<T> = SharedRef<T>;
type Arc<T> = SharedRef<T>;

/// A packet retained under the module's held-packet quota. Drop and access it
/// only during a task poll or cancellation, while host access is available.
/// A link returning ERR_BUSY leaves this exact lease available for retry.
pub struct Packet {
    ctx: TaskContext,
    handle: u32,
    len: usize,
}
impl Packet {
    pub fn handle(&self) -> u32 {
        self.handle
    }
    pub fn data(&self) -> Result<&[u8], i32> {
        if self.handle == 0 {
            return Err(ERR_ARG);
        }
        let (p, cap) = self.ctx.with_hw(|h| h.pkt_buf(self.handle))??;
        if self.len > cap {
            return Err(ERR_SIZE);
        }
        // The guard owns the retained lease and no mutable buffer is exposed.
        Ok(unsafe { core::slice::from_raw_parts(p, self.len) })
    }
    pub fn send(&mut self, dst: u32, component: i32, method: i32, id: u64) -> i32 {
        if self.handle == 0 {
            return ERR_ARG;
        }
        let rc = match self
            .ctx
            .with_hw(|h| h.send(self.handle, self.len, dst, component, method, id))
        {
            Ok(rc) => rc,
            Err(e) => return e,
        };
        if !(dst >= ADDR_LINK && rc == ERR_BUSY) {
            self.handle = 0;
        }
        rc
    }
}
impl Drop for Packet {
    fn drop(&mut self) {
        if self.handle != 0 {
            let result = self.ctx.with_hw(|h| h.pkt_free(self.handle));
            debug_assert!(result.is_ok(), "packet dropped outside task handler");
        }
    }
}

/// An owned incoming call. Pool-backed payloads remain in their original packet;
/// packetless payloads use fallibly allocated storage.
pub struct OwnedCall {
    pub component: i32,
    pub method: i32,
    pub value: i32,
    pub flags: u32,
    pub from: u32,
    pub id: u64,
    pub packet: Option<Packet>,
    bytes: Vec<u8>,
}
impl OwnedCall {
    pub fn data(&self) -> Result<&[u8], i32> {
        match &self.packet {
            Some(p) => p.data(),
            None => Ok(&self.bytes),
        }
    }
    pub fn view(&self) -> Result<CallView<'_>, i32> {
        Ok(CallView {
            component: self.component,
            method: self.method,
            value: self.value,
            flags: self.flags,
            from: self.from,
            id: self.id,
            data: self.data()?,
            pkt: self.packet.as_ref().map_or(0, Packet::handle),
        })
    }
    fn snapshot(ctx: &TaskContext, hw: &Hw, c: &CallView) -> Result<Self, i32> {
        let mut bytes = Vec::new();
        let packet = if c.pkt != 0 {
            let rc = hw.pkt_keep(c.pkt);
            if rc < 0 {
                return Err(rc);
            }
            Some(Packet {
                ctx: ctx.clone(),
                handle: c.pkt,
                len: c.data.len(),
            })
        } else {
            bytes
                .try_reserve_exact(c.data.len())
                .map_err(|_| ERR_NOMEM)?;
            bytes.extend_from_slice(c.data);
            None
        };
        Ok(Self {
            component: c.component,
            method: c.method,
            value: c.value,
            flags: c.flags,
            from: c.from,
            id: c.id,
            packet,
            bytes,
        })
    }
}
// Reuse the incoming borrowed packet when unique: a full held quota must not
// prevent its own rejection response. Sending consumes that borrowed lease; the
// runtime releases it after the handler if a blocked link retained it instead.
fn reply_busy(hw: &Hw, call: &CallView) -> i32 {
    if call.pkt != 0 {
        if let Ok((p, cap)) = hw.pkt_buf(call.pkt) {
            if cap >= 1 {
                unsafe {
                    p.write(0x24);
                }
                return hw.send(call.pkt, 1, call.from, call.component, call.method, call.id);
            }
        }
    }
    hw.reply(call, &[0x24])
}

struct Alarm {
    method: i32,
    id: Cell<Option<i32>>,
}
struct Shared {
    hw: Cell<Option<Hw>>,
    inbox: RefCell<VecDeque<OwnedCall>>,
    alarms: RefCell<Vec<Rc<Alarm>>>,
    next_method: Cell<i32>,
    rejected: Cell<u32>,
}
struct Signal {
    awake: AtomicBool,
    polling: AtomicBool,
    vtable: RawWakerVTable,
}
// Each RawWaker owns one strong reference; its vtable is in that same stable
// allocation, so remains alive through clone/wake/drop, including retained clones.
unsafe fn clone_waker(p: *const ()) -> RawWaker {
    Arc::<Signal>::increment_strong_count(p.cast_mut().cast::<RefInner<Signal>>());
    raw_waker(p)
}
unsafe fn wake(p: *const ()) {
    let s = Arc::<Signal>::from_raw(p.cast_mut().cast::<RefInner<Signal>>());
    debug_assert!(
        s.polling.load(Ordering::Acquire),
        "wake outside task poll requires a runtime wake bridge"
    );
    s.awake.store(true, Ordering::Release);
}
unsafe fn wake_ref(p: *const ()) {
    let s = &(*p.cast::<RefInner<Signal>>()).value;
    debug_assert!(
        s.polling.load(Ordering::Acquire),
        "wake outside task poll requires a runtime wake bridge"
    );
    s.awake.store(true, Ordering::Release);
}
unsafe fn drop_waker(p: *const ()) {
    drop(Arc::<Signal>::from_raw(
        p.cast_mut().cast::<RefInner<Signal>>(),
    ));
}
unsafe fn raw_waker(p: *const ()) -> RawWaker {
    RawWaker::new(p, &(*p.cast::<RefInner<Signal>>()).value.vtable)
}
fn signal() -> Result<Arc<Signal>, i32> {
    // Construct pointer tables in instance RAM, never static flash data needing
    // load-time relocation. black_box keeps LLVM from promoting the table.
    Arc::try_new(Signal {
        awake: AtomicBool::new(false),
        polling: AtomicBool::new(false),
        vtable: RawWakerVTable::new(
            core::hint::black_box(clone_waker),
            core::hint::black_box(wake),
            core::hint::black_box(wake_ref),
            core::hint::black_box(drop_waker),
        ),
    })
}
/// Cloneable access to the task's current poll and owned incoming calls.
#[derive(Clone)]
pub struct TaskContext(Rc<Shared>);
impl TaskContext {
    /// Run a synchronous host operation during the current poll (or cancellation).
    /// Never retain the supplied `Hw` or pointers obtained from it in a future.
    pub fn with_hw<T>(&self, f: impl FnOnce(&Hw) -> T) -> Result<T, i32> {
        self.0.hw.get().map(|hw| f(&hw)).ok_or(ERR_ARG)
    }
    /// Refusals since this task started, including inbox, heap and packet quota.
    pub fn rejected_calls(&self) -> u32 {
        self.0.rejected.get()
    }
    pub fn next_call(&self) -> NextCall {
        NextCall(self.clone())
    }
    pub fn sleep(&self, ms: u32) -> Sleep {
        Sleep {
            ctx: self.clone(),
            ms,
            deadline: None,
            alarm: None,
        }
    }
}
pub struct NextCall(TaskContext);
impl Future for NextCall {
    type Output = OwnedCall;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0 .0.inbox.borrow_mut().pop_front() {
            Some(c) => Poll::Ready(c),
            None => Poll::Pending,
        }
    }
}
pub struct Sleep {
    ctx: TaskContext,
    ms: u32,
    deadline: Option<u64>,
    alarm: Option<Rc<Alarm>>,
}
impl Future for Sleep {
    type Output = Result<(), i32>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let s = self.get_mut();
        if s.ms == 0 {
            return Poll::Ready(Ok(()));
        }
        let now = match s.ctx.with_hw(|h| h.now_ms()) {
            Ok(now) => now,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let due = *s.deadline.get_or_insert(now.saturating_add(s.ms as u64));
        if now >= due {
            return Poll::Ready(Ok(()));
        }
        if s.alarm.is_none() {
            // Each live timer has a different reserved method. The runtime does
            // not include its reusable alarm slot id in an alarm call.
            let mut method = s.ctx.0.next_method.get();
            let alarms = s.ctx.0.alarms.borrow();
            while method == METHOD || alarms.iter().any(|a| a.method == method) {
                method = method.wrapping_add(1);
            }
            drop(alarms);
            s.ctx.0.next_method.set(method.wrapping_add(1));
            let alarm = match Rc::try_new(Alarm {
                method,
                id: Cell::new(None),
            }) {
                Ok(a) => a,
                Err(e) => return Poll::Ready(Err(e)),
            };
            let mut alarms = s.ctx.0.alarms.borrow_mut();
            if alarms.try_reserve(1).is_err() {
                return Poll::Ready(Err(ERR_NOMEM));
            }
            alarms.push(alarm.clone());
            s.alarm = Some(alarm);
        }
        let alarm = s.alarm.as_ref().unwrap();
        if alarm.id.get().is_none() {
            let remaining = due.saturating_sub(now).min(u32::MAX as u64) as u32;
            let id = match s
                .ctx
                .with_hw(|h| h.alarm_set(remaining, 0, 0, ALARM_EXACT, COMPONENT, alarm.method))
            {
                Ok(id) => id,
                Err(e) => return Poll::Ready(Err(e)),
            };
            if id < 0 {
                return Poll::Ready(Err(id));
            }
            alarm.id.set(Some(id));
        }
        Poll::Pending
    }
}
impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(alarm) = self.alarm.take() {
            self.ctx
                .0
                .alarms
                .borrow_mut()
                .retain(|a| !Rc::ptr_eq(a, &alarm));
            if let Some(id) = alarm.id.take() {
                // Only an actual notification retires a slot; a clock reading
                // alone cannot establish that an alarm's handler ran.
                let result = self.ctx.with_hw(|h| h.alarm_cancel(id));
                debug_assert!(result.is_ok(), "sleep dropped outside task handler");
            }
        }
    }
}
struct Running {
    future: *mut (),
    poll: unsafe fn(*mut (), &mut Context<'_>) -> Poll<i32>,
    drop_future: unsafe fn(*mut ()),
    ctx: TaskContext,
    signal: Arc<Signal>,
    repoll: Option<i32>,
}
impl Drop for Running {
    fn drop(&mut self) {
        unsafe {
            (self.drop_future)(self.future);
        }
    }
}
unsafe fn poll_future<F: Future<Output = i32>>(p: *mut (), cx: &mut Context<'_>) -> Poll<i32> {
    // start allocates F once in a Box; the pointee never moves until drop_future.
    // The erased pointer and these poll/drop functions are installed for the same F.
    Pin::new_unchecked(&mut *p.cast::<F>()).poll(cx)
}
unsafe fn drop_future<F>(p: *mut ()) {
    drop(Box::from_raw(p.cast::<F>()));
}
/// Zero-valid module state. Route HOST_STOP through `handle`; use `cancel` before
/// returning an unrelated error or replacing the task. All destruction must occur
/// inside the handler allocator context.
#[derive(Default)]
pub struct Task(Option<Box<Running>>, u32, u32);
impl Task {
    pub fn start<F: Future<Output = i32> + 'static>(
        &mut self,
        hw: &Hw,
        make: impl FnOnce(TaskContext) -> F,
    ) -> i32 {
        self.cancel(hw);
        let mut inbox = VecDeque::new();
        if inbox.try_reserve_exact(INBOX).is_err() {
            return ERR_NOMEM;
        }
        let shared = match Rc::try_new(Shared {
            hw: Cell::new(Some(*hw)),
            inbox: RefCell::new(inbox),
            alarms: RefCell::new(Vec::new()),
            next_method: Cell::new(METHOD + 1),
            rejected: Cell::new(0),
        }) {
            Ok(shared) => shared,
            Err(e) => return e,
        };
        let ctx = TaskContext(shared);
        let signal = match signal() {
            Ok(s) => s,
            Err(e) => return e,
        };
        let future = match try_box(make(ctx.clone())) {
            Ok(f) => Box::into_raw(f).cast(),
            Err(e) => {
                ctx.0.hw.set(None);
                return e;
            }
        };
        let running = match try_box(Running {
            future,
            poll: core::hint::black_box(poll_future::<F>),
            drop_future: core::hint::black_box(drop_future::<F>),
            ctx: ctx.clone(),
            signal,
            repoll: None,
        }) {
            Ok(r) => r,
            Err(e) => {
                ctx.0.hw.set(None);
                return e;
            }
        };
        ctx.0.hw.set(None);
        self.0 = Some(running);
        self.poll(hw, None)
    }
    /// Number of incoming calls refused by this task (inbox, heap, or packet quota).
    pub fn rejected_calls(&self) -> u32 {
        self.1
    }
    /// Rejected requests for which no BUSY reply could be delivered.
    pub fn failed_replies(&self) -> u32 {
        self.2
    }
    /// Retain an incoming call, then poll once. Reserved alarms resume timers.
    /// HOST_STOP automatically drops the future and all retained resources.
    pub fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_STOP) {
            self.cancel(hw);
            return RET_DONE as i32;
        }
        self.poll(hw, Some(call))
    }
    fn poll(&mut self, hw: &Hw, call: Option<&CallView>) -> i32 {
        let Some(r) = &mut self.0 else {
            return RET_CONTINUE as i32;
        };
        r.ctx.0.hw.set(Some(*hw));
        if let Some(c) = call {
            if c.component == COMPONENT {
                if c.method == METHOD {
                    r.repoll = None;
                }
                for alarm in r.ctx.0.alarms.borrow().iter() {
                    if alarm.method == c.method {
                        alarm.id.set(None);
                    }
                }
            } else {
                let mut q = r.ctx.0.inbox.borrow_mut();
                let snapshot = if q.len() == INBOX {
                    Err(ERR_BUSY)
                } else {
                    OwnedCall::snapshot(&r.ctx, hw, c)
                };
                match snapshot {
                    Ok(call) => q.push_back(call),
                    Err(_) => {
                        self.1 = self.1.saturating_add(1);
                        r.ctx
                            .0
                            .rejected
                            .set(r.ctx.0.rejected.get().saturating_add(1));
                        // A CBOR negative integer, using existing reply routing
                        // and correlation. Refusal never terminates the task.
                        if c.wants_response() && reply_busy(hw, c) < 0 {
                            self.2 = self.2.saturating_add(1);
                        }
                    }
                }
            }
        }
        if let Some(a) = r.repoll.take() {
            hw.alarm_cancel(a);
        }
        r.signal.awake.store(false, Ordering::Release);
        let waker = unsafe { Waker::from_raw(raw_waker(Arc::into_raw(r.signal.clone()).cast())) };
        r.signal.polling.store(true, Ordering::Release);
        let result = unsafe { (r.poll)(r.future, &mut Context::from_waker(&waker)) };
        r.signal.polling.store(false, Ordering::Release);
        r.ctx.0.hw.set(None);
        match result {
            Poll::Ready(rc) => {
                self.cancel(hw);
                rc
            }
            Poll::Pending => {
                if r.signal.awake.swap(false, Ordering::AcqRel) {
                    let a = hw.alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, METHOD);
                    if a < 0 {
                        self.cancel(hw);
                        return a;
                    }
                    r.repoll = Some(a);
                }
                RET_CONTINUE as i32
            }
        }
    }
    pub fn cancel(&mut self, hw: &Hw) {
        if let Some(mut r) = self.0.take() {
            r.ctx.0.hw.set(Some(*hw));
            if let Some(a) = r.repoll.take() {
                hw.alarm_cancel(a);
            }
            // Keep a context alive until the future's destructors finish.
            let ctx = r.ctx.clone();
            drop(r);
            ctx.0.inbox.borrow_mut().clear();
            ctx.0.hw.set(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Host, ABI_VERSION, HOST_COMPONENT};
    use alloc::rc::Rc as TestRc;
    use core::{
        cell::UnsafeCell,
        ffi::{c_int, c_void},
    };

    #[derive(Clone, Copy)]
    struct Timer {
        due: u64,
        method: i32,
    }
    struct Fixture {
        now: Cell<u64>,
        timers: RefCell<[Option<Timer>; 4]>,
        bytes: UnsafeCell<[u8; 16]>,
        held: Cell<bool>,
        kept: Cell<u32>,
        freed: Cell<u32>,
        blocked: Cell<bool>,
        sends: RefCell<Vec<(u32, Vec<u8>)>>,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                now: Cell::new(0),
                timers: RefCell::new([None; 4]),
                bytes: UnsafeCell::new([42; 16]),
                held: Cell::new(false),
                kept: Cell::new(0),
                freed: Cell::new(0),
                blocked: Cell::new(false),
                sends: RefCell::new(Vec::new()),
            }
        }
        fn host(&self) -> Host {
            let mut h: Host = unsafe { core::mem::zeroed() };
            h.abi_version = ABI_VERSION;
            h.size = core::mem::size_of::<Host>() as u32;
            h.user = self as *const Self as *mut c_void;
            h.now_ms = Some(now);
            h.alarm_set = Some(set);
            h.alarm_cancel = Some(cancel);
            h.pkt_keep = Some(keep);
            h.pkt_free = Some(free);
            h.pkt_alloc = Some(allocate);
            h.pkt_buf = Some(buffer);
            h.send = Some(send);
            h
        }
        fn fire(&self, slot: usize, now: u64) -> CallView<'static> {
            self.now.set(now);
            let timer = self.timers.borrow_mut()[slot].take().expect("armed timer");
            call(COMPONENT, timer.method)
        }
    }
    unsafe fn fixture<'a>(p: *mut c_void) -> &'a Fixture {
        &*p.cast::<Fixture>()
    }
    unsafe extern "C" fn now(p: *mut c_void) -> u64 {
        fixture(p).now.get()
    }
    unsafe extern "C" fn set(
        p: *mut c_void,
        delay: u32,
        _: u32,
        _: u32,
        _: u32,
        _: c_int,
        method: c_int,
    ) -> c_int {
        let f = fixture(p);
        let mut timers = f.timers.borrow_mut();
        let Some(i) = timers.iter().position(Option::is_none) else {
            return ERR_NOMEM;
        };
        timers[i] = Some(Timer {
            due: f.now.get() + delay as u64,
            method,
        });
        i as i32
    }
    unsafe extern "C" fn cancel(p: *mut c_void, id: c_int) -> c_int {
        fixture(p).timers.borrow_mut()[id as usize] = None;
        0
    }
    unsafe extern "C" fn allocate(p: *mut c_void, out: *mut u32) -> i32 {
        fixture(p).held.set(true);
        *out = 7;
        0
    }
    unsafe extern "C" fn buffer(
        p: *mut c_void,
        pkt: u32,
        ptr: *mut *mut u8,
        cap: *mut usize,
    ) -> i32 {
        assert_eq!(pkt, 7);
        let f = fixture(p);
        assert!(f.held.get());
        *ptr = f.bytes.get().cast::<u8>();
        *cap = 16;
        0
    }
    unsafe extern "C" fn keep(p: *mut c_void, pkt: u32) -> i32 {
        assert_eq!(pkt, 7);
        let f = fixture(p);
        if f.held.replace(true) {
            return ERR_BUSY;
        }
        f.kept.set(f.kept.get() + 1);
        0
    }
    unsafe extern "C" fn free(p: *mut c_void, pkt: u32) -> i32 {
        assert_eq!(pkt, 7);
        let f = fixture(p);
        assert!(f.held.replace(false));
        f.freed.set(f.freed.get() + 1);
        0
    }
    unsafe extern "C" fn send(
        p: *mut c_void,
        pkt: u32,
        len: usize,
        _: u32,
        _: i32,
        _: i32,
        _: u64,
    ) -> i32 {
        let f = fixture(p);
        assert!(f.held.get());
        if f.blocked.replace(false) {
            return ERR_BUSY;
        }
        f.sends
            .borrow_mut()
            .push((pkt, (&*f.bytes.get())[..len].to_vec()));
        f.held.set(false);
        0
    }
    fn call(component: i32, method: i32) -> CallView<'static> {
        CallView {
            component,
            method,
            value: 0,
            flags: 0,
            from: 0,
            id: 0,
            data: &[],
            pkt: 0,
        }
    }
    fn hw(h: &Host) -> Hw {
        unsafe { Hw::new(h) }.unwrap()
    }

    #[test]
    fn early_alarm_rearms_remaining_time() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        assert_eq!(
            task.start(&hw, |ctx| async move {
                ctx.sleep(5).await.unwrap();
                RET_DONE as i32
            }),
            RET_CONTINUE as i32
        );
        let early = f.fire(0, 4);
        assert_eq!(task.handle(&hw, &early), RET_CONTINUE as i32);
        assert_eq!(f.timers.borrow()[0].unwrap().due, 5);
        let due = f.fire(0, 5);
        assert_eq!(task.handle(&hw, &due), RET_DONE as i32);
        assert!(f.timers.borrow().iter().all(Option::is_none));
    }

    #[test]
    fn concurrent_sleeps_dropping_one_preserves_the_other() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            let mut a = Some(Box::pin(ctx.sleep(5)));
            let mut b = Box::pin(ctx.sleep(10));
            core::future::poll_fn(|cx| {
                if ctx.with_hw(|h| h.now_ms()).unwrap() == 3 {
                    a = None;
                }
                if let Some(a) = &mut a {
                    assert!(a.as_mut().poll(cx).is_pending());
                }
                b.as_mut().poll(cx)
            })
            .await
            .unwrap();
            RET_DONE as i32
        });
        f.now.set(3);
        task.handle(&hw, &call(2, 1));
        assert!(f.timers.borrow()[0].is_none());
        assert_eq!(f.timers.borrow()[1].unwrap().due, 10);
        let due = f.fire(1, 10);
        assert_eq!(task.handle(&hw, &due), RET_DONE as i32);
    }

    #[test]
    fn dropped_fired_sleep_does_not_cancel_reused_slot() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            let mut stale = Box::pin(ctx.sleep(5));
            core::future::poll_fn(|cx| {
                assert!(stale.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            ctx.sleep(10).await.unwrap();
            let mut current = Box::pin(ctx.sleep(20));
            core::future::poll_fn(|cx| {
                assert!(current.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(stale);
            current.await.unwrap();
            RET_DONE as i32
        });
        let due = f.fire(0, 5);
        task.handle(&hw, &due);
        let due = f.fire(1, 10);
        task.handle(&hw, &due);
        assert_eq!(f.timers.borrow()[0].unwrap().due, 30);
        let due = f.fire(0, 30);
        assert_eq!(task.handle(&hw, &due), RET_DONE as i32);
    }

    #[test]
    fn overflow_replies_busy_counts_and_keeps_future_alive() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            ctx.sleep(1000).await.unwrap();
            RET_DONE as i32
        });
        for _ in 0..4 {
            assert_eq!(task.handle(&hw, &call(2, 1)), RET_CONTINUE as i32);
        }
        let mut c = call(2, 1);
        c.flags = 1;
        c.id = 123;
        assert_eq!(task.handle(&hw, &c), RET_CONTINUE as i32);
        assert_eq!(task.rejected_calls(), 1);
        assert_eq!(f.sends.borrow().as_slice(), &[(7, alloc::vec![0x24])]);
        assert!(f.timers.borrow()[0].is_some());
        assert_eq!(
            task.handle(&hw, &call(HOST_COMPONENT as i32, HOST_STOP as i32)),
            RET_DONE as i32
        );
        assert_eq!(task.rejected_calls(), 1);
        assert!(f.timers.borrow().iter().all(Option::is_none));
    }

    #[test]
    fn failed_overflow_reply_is_counted_without_stopping_task() {
        unsafe extern "C" fn no_packet(_: *mut c_void, _: *mut u32) -> i32 {
            ERR_NOMEM
        }
        let f = Fixture::new();
        let mut h = f.host();
        h.pkt_alloc = Some(no_packet);
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            ctx.sleep(1000).await.unwrap();
            RET_DONE as i32
        });
        for _ in 0..4 {
            task.handle(&hw, &call(2, 1));
        }
        let mut c = call(2, 1);
        c.flags = 1;
        assert_eq!(task.handle(&hw, &c), RET_CONTINUE as i32);
        assert_eq!(task.rejected_calls(), 1);
        assert_eq!(task.failed_replies(), 1);
        task.cancel(&hw);
        assert_eq!(task.failed_replies(), 1);
    }

    #[test]
    fn retained_packet_crosses_await_and_busy_preserves_exact_lease() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        let pointer = f.bytes.get() as usize;
        task.start(&hw, move |ctx| async move {
            let mut c = ctx.next_call().await;
            assert_eq!(c.data().unwrap().as_ptr() as usize, pointer);
            ctx.sleep(5).await.unwrap();
            assert_eq!(c.view().unwrap().pkt, 7);
            assert_eq!(c.data().unwrap(), &[42; 16]);
            let packet = c.packet.as_mut().unwrap();
            assert_eq!(packet.send(ADDR_LINK, 0, 0, 0), ERR_BUSY);
            assert_eq!(packet.handle(), 7);
            assert_eq!(packet.send(ADDR_LINK, 0, 0, 0), 0);
            assert_eq!(packet.handle(), 0);
            RET_DONE as i32
        });
        let mut c = call(2, 1);
        c.pkt = 7;
        c.data = unsafe { &*f.bytes.get() };
        assert_eq!(task.handle(&hw, &c), RET_CONTINUE as i32);
        assert_eq!(f.kept.get(), 1);
        assert!(f.held.get());
        f.blocked.set(true);
        let due = f.fire(0, 5);
        assert_eq!(task.handle(&hw, &due), RET_DONE as i32);
        assert_eq!(f.freed.get(), 0);
        assert_eq!(f.sends.borrow()[0].0, 7);
    }

    #[test]
    fn stop_drops_packet_waiting_in_future_without_manual_cancel() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            let _c = ctx.next_call().await;
            ctx.next_call().await;
            RET_DONE as i32
        });
        let mut c = call(2, 1);
        c.pkt = 7;
        c.data = unsafe { &*f.bytes.get() };
        task.handle(&hw, &c);
        assert!(f.held.get());
        task.handle(&hw, &call(HOST_COMPONENT as i32, HOST_STOP as i32));
        assert!(!f.held.get());
        assert_eq!(f.freed.get(), 1);
    }

    #[test]
    fn packet_quota_refusal_drops_only_the_incoming_call() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        task.start(&hw, |ctx| async move {
            ctx.next_call().await;
            RET_DONE as i32
        });
        // A separate held lease has already exhausted this fixture's quota.
        f.held.set(true);
        let mut c = call(2, 1);
        c.pkt = 7;
        assert_eq!(task.handle(&hw, &c), RET_CONTINUE as i32);
        assert_eq!(task.rejected_calls(), 1);
        assert_eq!(f.kept.get(), 0);
        assert_eq!(f.freed.get(), 0);
        task.cancel(&hw);
        assert_eq!(
            f.freed.get(),
            0,
            "the refused borrowed packet was never owned by the task"
        );
    }

    #[test]
    fn reference_count_overflow_is_checked_without_wrapping() {
        extern crate std;
        let reference = SharedRef::try_new(()).unwrap();
        unsafe { reference.0.as_ref() }
            .refs
            .store(isize::MAX as usize, Ordering::Relaxed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reference.clone()));
        assert!(result.is_err());
        assert_eq!(
            unsafe { reference.0.as_ref() }.refs.load(Ordering::Relaxed),
            isize::MAX as usize
        );
        unsafe { reference.0.as_ref() }
            .refs
            .store(1, Ordering::Relaxed);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "wake outside task poll requires a runtime wake bridge")]
    fn outside_poll_wake_has_a_debug_assertion() {
        let signal = signal().unwrap();
        let waker = unsafe { Waker::from_raw(raw_waker(Arc::into_raw(signal).cast())) };
        waker.wake_by_ref();
    }

    #[test]
    fn with_hw_outside_poll_is_an_error() {
        let f = Fixture::new();
        let h = f.host();
        let hw = hw(&h);
        let mut task = Task::default();
        let captured = TestRc::new(RefCell::new(None));
        let save = captured.clone();
        task.start(&hw, |ctx| {
            *save.borrow_mut() = Some(ctx.clone());
            async move {
                ctx.next_call().await;
                RET_DONE as i32
            }
        });
        assert_eq!(
            captured.borrow().as_ref().unwrap().with_hw(|h| h.now_ms()),
            Err(ERR_ARG)
        );
        task.cancel(&hw);
        drop(captured.borrow_mut().take());
    }
}
