//! Experimental bounded plaintext QUIC-lite endpoint on C-owned pool/links.
//! One serialized owner, one association, two echo streams; no radio ownership.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
mod bearer;
mod pool;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use bearer::{Bearer, Bridge};
use pool::{Buffer, Native, Pool};
use quic_lite::{nostd::NoStdRuntime, *};
use spin::Mutex;
use xip_abi::{
    cbor::{self, Item, Reader, Writer},
    *,
};
const COMPONENT: i32 = 48;
const RX: i32 = 100;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;
#[derive(Default)]
pub struct State {
    context: usize,
}
struct Echo {
    stream: QuicStream,
    bytes: Vec<u8>,
    at: usize,
    fin: bool,
}
struct Core {
    // Field order is intentional: runtime/callback queues and all buffer owners
    // drop before the boxed pool shim whose reference the library calls static.
    runtime: Box<NoStdRuntime<Pool>>,
    bridges: Vec<Arc<Mutex<Bridge>>>,
    echoes: Vec<Echo>,
    pool: Box<Pool>,
}
impl Core {
    fn new(native: Native) -> Result<Self, i32> {
        let pool = Box::new(Pool { native });
        // SAFETY: this boxed pointee never moves. No Rust callback or pool ref
        // is registered with SDK code or escapes Core. runtime + bridges drop
        // first; State detaches native RX/completions before dropping Core.
        let scoped_static = unsafe { &*(pool.as_ref() as *const Pool) };
        let limits = ConnectionLimits::with_receive_profile(4096, 2048, 2);
        let mut runtime =
            NoStdRuntime::try_new_boxed(scoped_static, limits).map_err(|_| ERR_NOMEM)?;
        runtime
            .set_limits(NodeLimits {
                max_associations: 1,
                max_routes: 8,
                relay_control_packet_reserve: 1,
            })
            .map_err(|_| ERR_ARG)?;
        let mut limits = AssociationLimits::embedded();
        limits.connection = ConnectionLimits::with_receive_profile(4096, 2048, 2);
        limits.history_packets = 2;
        limits.transmission_records = 4;
        limits.max_pending_streams = 2;
        limits.max_queued_chunks_per_stream = 1;
        limits.max_buffered_stream_bytes = 4096;
        limits.duplication_budget_bytes = 1;
        runtime
            .set_default_association_limits(limits)
            .map_err(|_| ERR_ARG)?;
        Ok(Self {
            runtime,
            bridges: Vec::new(),
            echoes: Vec::new(),
            pool,
        })
    }
    fn bridge(&mut self, link: u32) -> Result<Arc<Mutex<Bridge>>, i32> {
        if let Some(b) = self.bridges.iter().find(|b| b.lock().link == link) {
            return Ok(b.clone());
        }
        if self.bridges.len() >= 4 {
            return Err(ERR_BUSY);
        }
        let native = self.pool.native;
        let mut info: LinkInfo = unsafe { core::mem::zeroed() };
        info.size = core::mem::size_of::<LinkInfo>() as u32;
        let rc = unsafe {
            native
                .hw
                .link()
                .and_then(|l| l.info)
                .ok_or(ERR_UNSUPPORTED)?(native.hw.ctx(), link, &mut info)
        };
        if rc < 0 {
            return Err(rc);
        }
        if info.provider != LINK_NATIVE {
            return Err(ERR_UNSUPPORTED);
        }
        let len = info
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(info.name.len());
        let name = core::str::from_utf8(&info.name[..len]).map_err(|_| ERR_ARG)?;
        let name = BearerName::new(name).ok_or(ERR_ARG)?;
        let bridge = Arc::new(Mutex::new(Bridge {
            link,
            context: None,
            pending: Vec::new(),
        }));
        let bearer = Bearer {
            native,
            bridge: bridge.clone(),
            info: BearerInfo {
                name,
                max_packet_size: (info.mtu as usize).min(1100),
                prefix_required: 80,
                suffix_required: 24,
                requires_packet_encryption: false,
                secure_link: false,
                nominal_bitrate_bps: u64::from(info.rate_kbps) * 1000,
                local_mac: None,
            },
        };
        self.bridges.try_reserve(1).map_err(|_| ERR_NOMEM)?;
        self.runtime.add_bearer(bearer).map_err(|_| ERR_ARG)?;
        self.bridges.push(bridge.clone());
        Ok(bridge)
    }
    fn receive(&mut self, call: &CallView) -> Result<(), i32> {
        let bridge = self.bridge(call.from)?;
        let context = bridge.lock().context.clone().ok_or(ERR_IO)?;
        let rc = self.pool.native.hw.pkt_keep(call.pkt);
        if rc < 0 {
            return Err(rc);
        }
        let buffer = Buffer::owned(self.pool.native, call.pkt)?;
        let base = buffer.as_ref().as_ptr() as usize;
        let start = (call.data.as_ptr() as usize)
            .checked_sub(base)
            .ok_or(ERR_ARG)?;
        let packet =
            OwnedPacket::new(buffer, start..start + call.data.len()).map_err(|_| ERR_ARG)?;
        let peer = if call.id == 0 {
            PORT_QUIC as u64
        } else {
            call.id
        };
        context.enqueue_packet(
            PacketMeta {
                bearer: context.bearer(),
                peer_l2_address: PeerL2Address::new(peer).ok_or(ERR_ARG)?,
                received_at_us: self.pool.native.hw.now_us().unwrap_or(call.time_us as u64),
            },
            packet,
        );
        Ok(())
    }
    fn poll_completions(&mut self) -> Result<(), i32> {
        let native = self.pool.native;
        for _ in 0..4 {
            let mut done = QuicTxCompletion {
                size: core::mem::size_of::<QuicTxCompletion>() as u32,
                ..Default::default()
            };
            let rc =
                unsafe { native.table.poll_tx.ok_or(ERR_UNSUPPORTED)?(native.hw.ctx(), &mut done) };
            if rc == 0 || rc == ERR_BUSY {
                break;
            }
            if rc < 0 {
                return Err(rc);
            }
            if let Some(b) = self.bridges.iter().find(|b| b.lock().link == done.link) {
                b.lock().poll(native, done)?;
            } else {
                native.hw.pkt_free(done.packet);
            }
        }
        for b in &self.bridges {
            if let Some(c) = &b.lock().context {
                c.send_ready();
            }
        }
        Ok(())
    }
    // Keep timer/retirement scratch out of the live ingress call chain. The
    // shared worker cannot retain both phase frames while admitting a peer.
    #[inline(never)]
    fn advance_clock(&mut self, now: u64) {
        let _ = self.runtime.advance_time(now);
    }
    #[inline(never)]
    fn progress_ingress(&mut self) {
        for _ in 0..4 {
            match self.runtime.progress() {
                Ok(true) => {}
                Ok(false) | Err(_) => break,
            }
        }
    }
    fn progress(&mut self, now: u64) -> Result<(), i32> {
        self.poll_completions()?;
        self.advance_clock(now);
        self.progress_ingress();
        while self.runtime.next_association().is_some() {}
        // At most two application chunks outside the core. Each Vec is moved
        // from its existing owned receive chunk, not defensively cloned.
        if self.echoes.iter().all(|e| e.bytes.is_empty()) {
            if let Some(chunk) = self.runtime.next_stream_chunk() {
                if let Some(e) = self
                    .echoes
                    .iter_mut()
                    .find(|e| e.stream.same_stream(&chunk.stream))
                {
                    if e.at == e.bytes.len() {
                        e.bytes = chunk.bytes;
                        e.at = 0;
                        e.fin = chunk.fin;
                    } else {
                        return Err(ERR_BUSY);
                    }
                } else {
                    if self.echoes.len() >= 2 {
                        return Err(ERR_UNSUPPORTED);
                    }
                    self.echoes.try_reserve(1).map_err(|_| ERR_NOMEM)?;
                    self.echoes.push(Echo {
                        stream: chunk.stream,
                        bytes: chunk.bytes,
                        at: 0,
                        fin: chunk.fin,
                    });
                }
            }
        }
        for e in &mut self.echoes {
            if e.at < e.bytes.len() {
                let result = if e.fin {
                    self.runtime
                        .write_stream_and_finish(&mut e.stream, &e.bytes[e.at..])
                } else {
                    self.runtime.write_stream(&mut e.stream, &e.bytes[e.at..])
                };
                if let Ok(n) = result {
                    e.at += n;
                }
            } else if e.fin {
                let _ = self.runtime.finish_stream(&mut e.stream);
            }
        }
        // Retain stable send offsets for peer-opened streams until association
        // retirement. Fully sent buffers are freed, not kept as scratch capacity.
        for e in &mut self.echoes {
            if e.at == e.bytes.len() {
                e.bytes = Vec::new();
                e.at = 0;
            }
        }
        Ok(())
    }
    fn in_flight(&self) -> bool {
        self.bridges.iter().any(|b| !b.lock().pending.is_empty())
    }
}
struct Context {
    native: Native,
    core: Option<Core>,
    alarm: i32,
    received: u64,
    errors: u64,
    last_error: i32,
    last_rx_us: u64,
}
impl Context {
    fn tick(&mut self) {
        let now = self.native.hw.now_us().unwrap_or(0);
        if let Some(c) = self.core.as_mut() {
            if let Err(rc) = c.progress(now) {
                self.errors += 1;
                self.last_error = rc;
            }
            if c.runtime.telemetry().live_associations == 0 {
                c.echoes.clear();
            }
            if c.runtime.telemetry().live_associations == 0 && !c.in_flight() {
                self.core = None;
            }
        }
        if self.alarm >= 0 {
            self.native.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        if let Some(c) = &self.core {
            let next = c
                .runtime
                .next_deadline()
                .unwrap_or(now + 10000)
                .min(now + 10000);
            let delay = next.saturating_sub(now).div_ceil(1000).max(1) as u32;
            self.alarm = self
                .native
                .hw
                .alarm_set(delay, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            if let Some(f) = self.native.hw.power().and_then(|p| p.awake_for) {
                unsafe {
                    f(self.native.hw.ctx(), 1000);
                }
            }
        }
    }
    fn reset(&mut self) -> Result<(), i32> {
        if self.alarm >= 0 {
            self.native.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        unsafe {
            self.native.table.detach.ok_or(ERR_UNSUPPORTED)?(self.native.hw.ctx());
        }
        self.core = None;
        let rc =
            unsafe { self.native.table.attach.ok_or(ERR_UNSUPPORTED)?(self.native.hw.ctx(), RX) };
        if rc < 0 {
            Err(rc)
        } else {
            Ok(())
        }
    }
}
impl State {
    fn start(&mut self, hw: &Hw) -> Result<(), i32> {
        if self.context != 0 {
            return Err(ERR_BUSY);
        }
        let native = Native::get(*hw)?;
        let rc = unsafe { native.table.attach.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), RX) };
        if rc < 0 {
            return Err(rc);
        }
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Context>())
            .cast::<Context>();
        if ptr.is_null() {
            unsafe {
                native.table.detach.unwrap()(hw.ctx());
            }
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Context {
                native,
                core: None,
                alarm: -1,
                received: 0,
                errors: 0,
                last_error: 0,
                last_rx_us: 0,
            });
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
        let c = unsafe { &mut *ptr };
        if c.alarm >= 0 {
            hw.alarm_cancel(c.alarm);
        }
        if let Some(f) = c.native.table.detach {
            unsafe {
                f(hw.ctx());
            }
        }
        unsafe {
            ptr.drop_in_place();
        }
        hw.mem_free(ptr.cast());
    }
}
impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_START) {
            return self.start(hw).map_or_else(|e| e, |_| RET_CONTINUE as i32);
        }
        if call.is_host(HOST_STOP) {
            self.stop(hw);
            return RET_CONTINUE as i32;
        }
        if self.context == 0 {
            return RET_CONTINUE as i32;
        }
        let cx = unsafe { &mut *(self.context as *mut Context) };
        if call.component == COMPONENT && call.method == RX && call.flags & CALL_LINK != 0 {
            cx.received += 1;
            cx.last_rx_us = hw.now_us().unwrap_or(0);
            if cx.core.is_none() {
                match Core::new(cx.native) {
                    Ok(c) => cx.core = Some(c),
                    Err(rc) => {
                        cx.errors += 1;
                        cx.last_error = rc;
                        return RET_CONTINUE as i32;
                    }
                }
            }
            if let Err(rc) = cx.core.as_mut().unwrap().receive(call) {
                cx.errors += 1;
                cx.last_error = rc;
            }
            cx.tick();
            return RET_CONTINUE as i32;
        }
        if call.component == COMPONENT && call.method == TICK && call.flags & CALL_LINK == 0 {
            cx.alarm = -1;
            cx.tick();
            return RET_CONTINUE as i32;
        }
        let envelope = call.flags & CALL_LINK != 0;
        if !envelope && !call.wants_response() {
            return RET_CONTINUE as i32;
        }
        let method = if envelope {
            cbor::get_int(call.data, 2).ok().flatten().unwrap_or(0) as i32
        } else {
            call.method
        };
        let mut out = [0; 256];
        let mut w = Writer::new(&mut out);
        if method == 1 {
            let n = cx
                .core
                .as_ref()
                .map_or(0, |c| c.runtime.telemetry().live_associations);
            w.map(7)
                .uint(1)
                .uint(n)
                .uint(2)
                .boolean(cx.core.is_some())
                .uint(3)
                .uint(cx.received)
                .uint(4)
                .uint(cx.errors)
                .uint(5)
                .int(cx.last_error as i64)
                .uint(6)
                .boolean(false)
                .uint(7)
                .uint(2);
        } else if method == 2 {
            match cx.reset() {
                Ok(()) => {
                    w.map(0);
                }
                Err(rc) => {
                    w.int(rc as i64);
                }
            }
        } else {
            w.int(ERR_UNSUPPORTED as i64);
        }
        let result = w.finish().unwrap();
        if envelope {
            let Some(mut id) = cbor::get(call.data, 3).ok().flatten() else {
                return RET_CONTINUE as i32;
            };
            let Some(Item::Uint(id)) = id.next() else {
                return RET_CONTINUE as i32;
            };
            let mut reply = [0; 300];
            let mut w = Writer::new(&mut reply);
            w.map(4)
                .uint(1)
                .uint(COMPONENT as u64)
                .uint(2)
                .int(method as i64)
                .uint(3)
                .uint(id)
                .uint(
                    if matches!(Reader::new(result).next(), Some(Item::Neg(_))) {
                        7
                    } else {
                        6
                    },
                )
                .raw(result);
            if let Some(data) = w.finish() {
                hw.message(call.from, 0, 0, call.id, data);
            }
        } else {
            hw.reply(call, result);
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
mod tests;
