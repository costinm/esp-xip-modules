//! Experimental bounded plaintext QUIC-lite endpoint on C-owned pool/links.
//! Local QUIC associations and opaque streams on native C-owned packets/links.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
mod bearer;
mod pool;
mod streams;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use bearer::{Bearer, Bridge};
use pool::{Buffer, Native, Pool};
use quic_lite::{nostd::NoStdRuntime, *};
use spin::Mutex;
use streams::Session;
use xip_abi::{cbor::Writer, *};
const COMPONENT: i32 = 48;
const RX: i32 = 100;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;
// Credit and retention ceilings, not eagerly allocated packet/receive buffers.
const RECEIVE_LIMITS: ConnectionLimits = ConnectionLimits::with_receive_profile(32*1024,16*1024,2);
const HISTORY_PACKETS: usize = 6;
const TRANSMISSION_RECORDS: usize = 12;
const BUFFERED_STREAM_BYTES: usize = 32*1024;
#[derive(Default)]
pub struct State {
    context: usize,
}
struct Core {
    // Field order is intentional: runtime/callback queues and all buffer owners
    // drop before the boxed pool shim whose reference the library calls static.
    runtime: Box<NoStdRuntime<Pool>>,
    bridges: Vec<Arc<Mutex<Bridge>>>,
    sessions: Vec<Session>,
    streams: Option<&'static Stream>,
    pool: Box<Pool>,
}
impl Drop for Core {
    fn drop(&mut self) {
        if let Some(close) = self.pool.native.hw.link().and_then(|l| l.close) {
            for bridge in &self.bridges {
                unsafe { close(self.pool.native.hw.ctx(), bridge.lock().link); }
            }
        }
    }
}
impl Core {
    // Keep cold construction temporaries off the receive/dispatch stack.
    #[inline(never)]
    fn new(native: Native) -> Result<Self, i32> {
        let pool = Box::new(Pool { native });
        // SAFETY: this boxed pointee never moves. No Rust callback or pool ref
        // is registered with SDK code or escapes Core. runtime + bridges drop
        // first; State detaches native RX/completions before dropping Core.
        let scoped_static = unsafe { &*(pool.as_ref() as *const Pool) };
        let limits = RECEIVE_LIMITS;
        let mut runtime =
            NoStdRuntime::try_new_boxed(scoped_static, limits).map_err(|_| ERR_NOMEM)?;
        runtime
            .set_limits(NodeLimits {
                max_associations: 1,
                max_routes: 8, // Local association/path entries; no relay resolver is installed.
                relay_control_packet_reserve: 1,
            })
            .map_err(|_| ERR_ARG)?;
        let mut limits = AssociationLimits::embedded();
        limits.connection = RECEIVE_LIMITS;
        limits.history_packets = HISTORY_PACKETS;
        limits.transmission_records = TRANSMISSION_RECORDS;
        limits.max_pending_streams = 2;
        limits.max_queued_chunks_per_stream = 1;
        limits.max_buffered_stream_bytes = BUFFERED_STREAM_BYTES;
        limits.duplication_budget_bytes = 1;
        runtime
            .set_default_association_limits(limits)
            .map_err(|_| ERR_ARG)?;
        Ok(Self {
            runtime,
            bridges: Vec::new(),
            sessions: Vec::new(),
            streams: native.hw.stream(),
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
        let table = native.hw.link().ok_or(ERR_UNSUPPORTED)?;
        let held = unsafe { table.open.ok_or(ERR_UNSUPPORTED)?(
            native.hw.ctx(), info.name.as_ptr().cast(), COMPONENT, LINK_EGRESS) };
        if held < 0 { return Err(held); }
        if held as u32 != link {
            if let Some(close) = table.close { unsafe { close(native.hw.ctx(), held as u32); } }
            return Err(ERR_IO);
        }
        if self.runtime.add_bearer(bearer).is_err() {
            if let Some(close) = table.close { unsafe { close(native.hw.ctx(), link); } }
            return Err(ERR_ARG);
        }
        self.bridges.push(bridge.clone());
        Ok(bridge)
    }
    fn receive(&mut self, call: &CallView, owned: xip_abi::Packet) -> Result<(), i32> {
        let bridge = self.bridge(call.from)?;
        let context = bridge.lock().context.clone().ok_or(ERR_IO)?;
        let buffer=Buffer::owned(self.pool.native,owned.into_raw())?;
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
    fn advance_clock(&mut self, now: u64)->Result<(),i32> {
        match self.runtime.advance_time(now){Ok(_)=>Ok(()),Err(e) if e.is_retryable()=>Ok(()),Err(_)=>Err(ERR_IO)}
    }
    #[inline(never)]
    fn progress_ingress(&mut self)->Result<(),i32> {
        let native = self.pool.native;
        let table = self.streams.ok_or(ERR_UNSUPPORTED)?;
        let sessions = &mut self.sessions;
        // Retry refused suffixes even if receive credit has stopped the peer.
        // The node owns retained bytes; the adapter never builds a chunk queue.
        for session in sessions.iter_mut().filter(|s| s.input.needs_resume && !s.closed) {
            let stream = &session.stream;
            match self.runtime.resume_stream_with(stream, |_, _, fin, bytes| {
                match session.input.feed(native, table, bytes, fin) {
                    Ok(n) => Ok(n),
                    Err(rc) => { session.input.input_error = Some(rc); Ok(0) }
                }
            }) {
                Ok(_) => {},
                Err(e) if e.is_retryable() => {},
                Err(_) => return Err(ERR_IO),
            }
        }
        for _ in 0..4 {
            let result = self.runtime.progress_with_stream(|stream, _, fin, bytes| {
                let at = match sessions.iter().position(|s| s.stream.same_stream(&stream)) {
                    Some(i) => i,
                    None => {
                        if sessions.len() >= 2 { return Err(Error::Invalid); }
                        sessions.try_reserve(1).map_err(|_| Error::HistoryFull)?;
                        sessions.push(Session::new(stream));
                        sessions.len()-1
                    }
                };
                match sessions[at].input.feed(native, table, bytes, fin) {
                    Ok(n) => Ok(n),
                    Err(rc) => { sessions[at].input.input_error = Some(rc); Ok(0) }
                }
            });
            match result {
                Ok(true) => {},
                Ok(false)=>break,
                Err(e) if e.is_retryable()=>break,
                Err(_)=>return Err(ERR_IO),
            }
        }
        Ok(())
    }
    fn progress(&mut self, now: u64) -> Result<(), i32> {
        self.advance_clock(now)?;
        self.progress_ingress()?;
        self.progress_egress()
    }
    // Keep egress bookkeeping off the ingress call chain. StreamPacket is
    // handle/range metadata; encoded wire bytes are written into a pool lease.
    #[inline(never)]
    fn progress_egress(&mut self) -> Result<(), i32> {
        while self.runtime.next_association().is_some() {}
        let table = self.streams.ok_or(ERR_UNSUPPORTED)?;
        for s in &mut self.sessions {
            if s.closed {
                continue;
            }
            if let Some(rc) = s.input.input_error.take() {
                s.close(self.pool.native, table);
                let _ = self.runtime.finish_stream(&mut s.stream);
                return Err(rc);
            }
            if s.input.handle != 0 && s.output.is_none() && self.pool.native.info()?.available > 2 {
                let mut out = StreamPacket::default();
                let rc = unsafe {
                    table.take.ok_or(ERR_UNSUPPORTED)?(
                        self.pool.native.hw.ctx(),
                        s.input.handle,
                        &mut out,
                    )
                };
                if rc == 1 {
                    let buffer = Buffer::owned(self.pool.native, out.packet)?;
                    let packet = OwnedPacket::new(
                        buffer,
                        PKT_HEADROOM as usize..PKT_HEADROOM as usize + out.len as usize,
                    )
                    .map_err(|_| ERR_ARG)?;
                    s.output = Some(packet.try_erase().map_err(|_| ERR_ARG)?);
                    s.output_fin = out.fin != 0;
                } else if rc == ERR_NOTFOUND {
                    // The consumer closed/stopped: propagate EOF to the peer,
                    // retaining the stream while reliable FIN admission is busy.
                    match self.runtime.finish_stream(&mut s.stream) {
                        Ok(()) => s.close(self.pool.native, table),
                        Err(e) if e.is_retryable() => {},
                        Err(_) => s.close(self.pool.native, table),
                    }
                    continue;
                } else if rc < 0 && rc != ERR_BUSY {
                    s.close(self.pool.native, table);
                    continue;
                }
            }
            if let Some(packet) = s.output.as_ref() {
                let remaining = packet.bytes().len() - s.output_at;
                let count = remaining
                    .min(DEFAULT_MAX_STREAM_PAYLOAD)
                    .min(self.runtime.stream_send_window(&s.stream).unwrap_or(0) as usize);
                let result = if remaining == 0 {
                    self.runtime.finish_stream(&mut s.stream)
                } else if count == 0 {
                    continue;
                } else {
                    // Reliable history retains ranges of this source lease,
                    // avoiding a separate retransmission-payload heap copy.
                    // quic-lite still copies each range into its encoded wire
                    // packet; the socket API also copies on submission.
                    let (buffer, range) = packet.clone_shared().into_parts();
                    let start = range.start + s.output_at;
                    let part =
                        OwnedPacket::new(buffer, start..start + count).map_err(|_| ERR_ARG)?;
                    self.runtime
                        .write_stream_owned_packet(
                            &mut s.stream,
                            part,
                            s.output_fin && count == remaining,
                        )
                        .map(|n| {
                            s.output_at += n;
                        })
                        .map_err(|(e, _lease)| e)
                };
                match result {
                    Ok(()) if s.output_at == packet.bytes().len() => {
                        s.output = None;
                        s.output_at = 0;
                        unsafe {
                            table.written.ok_or(ERR_UNSUPPORTED)?(
                                self.pool.native.hw.ctx(),
                                s.input.handle,
                            );
                        }
                        if s.output_fin {
                            s.close(self.pool.native, table);
                        }
                    }
                    Ok(()) => {}
                    Err(error) if error.is_retryable() => {}
                    Err(_) => {
                        s.close(self.pool.native, table);
                        return Err(ERR_IO);
                    }
                }
            }
        }
        // Reliable history is owned by the node. Completed application handles
        // need no lifetime reservation; the peer may open subsequent streams.
        self.sessions.retain(|s|!s.closed);
        Ok(())
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
    stats: Option<Box<NodeTelemetry>>,
}
impl Context {
    fn configure_stats(&mut self, fields: &[u8]) -> Result<(), i32> {
        match cbor::get(fields, 1).map_err(|_| ERR_ARG)? {
            None => {},
            Some(mut r) => match r.next() {
                Some(cbor::Item::Bool(true)) => self.stats = Some(Box::new(NodeTelemetry::default())),
                Some(cbor::Item::Bool(false)) => self.stats = None,
                _ => return Err(ERR_ARG),
            },
        }
        Ok(())
    }
    fn sample_stats(&mut self) {
        if let (Some(saved), Some(core)) = (&mut self.stats, &self.core) {
            let current = core.runtime.telemetry();
            if current.live_associations != 0 { **saved = current; }
        }
    }
    fn write_stats(&self, w: &mut Writer) {
        let current = self.core.as_ref().map(|c| c.runtime.telemetry()).unwrap_or_default();
        let t = if current.live_associations != 0 { current }
            else { self.stats.as_deref().copied().unwrap_or_default() };
        w.map(15).uint(1).boolean(self.stats.is_some())
            .uint(2).boolean(current.live_associations != 0)
            .uint(3).uint(t.received_packets).uint(4).uint(t.sent_packets)
            .uint(5).uint(t.duplicate_packets).uint(6).uint(t.out_of_order_packets)
            .uint(7).uint(t.inferred_missing_packets).uint(8).uint(t.retransmitted_packets)
            .uint(9).uint(t.loss_events).uint(10).uint(t.submission_failures)
            .uint(11).uint(t.received_stream_packets).uint(12).uint(t.received_control_packets)
            .uint(13).uint(t.sent_stream_packets).uint(14).uint(t.sent_control_packets)
            .uint(15).boolean(t.live_associations != 0);
    }
    fn tick(&mut self) {
        let now = self.native.hw.now_us().unwrap_or(0);
        self.sample_stats();
        if let Some(c) = self.core.as_mut() {
            if let Err(rc) = c.progress(now) {
                self.errors += 1;
                self.last_error = rc;
            }
            if let Some(saved) = &mut self.stats {
                let current = c.runtime.telemetry();
                if current.live_associations != 0 { **saved = current; }
            }
            if c.runtime.telemetry().live_associations == 0 {
                if let Some(t) = c.streams {
                    for s in &mut c.sessions {
                        s.close(c.pool.native, t);
                    }
                }
                c.sessions.clear();
            }
            if c.runtime.telemetry().live_associations == 0 {
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
        if let Some(c) = &mut self.core {
            if let Some(t) = c.streams {
                for s in &mut c.sessions {
                    s.close(c.pool.native, t);
                }
            }
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
                stats: None,
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
    fn handle_owned(&mut self,hw:&Hw,call:&CallView,packet:Option<xip_abi::Packet>)->i32 {
        if call.pkt!=0 && call.flags & CALL_LINK_EVENT==0 && self.context!=0 {
            let cx=unsafe {&mut *(self.context as *mut Context)};
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
            if let Err(rc) = cx.core.as_mut().unwrap().receive(call,packet.unwrap()) {
                cx.errors += 1;
                cx.last_error = rc;
            }
            cx.tick();
            return RET_CONTINUE as i32;
        }
        }
        self.handle(hw,call)
    }
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
        if call.flags & CALL_LINK_EVENT != 0 {
            if let Some(c)=&cx.core {
                for b in &c.bridges { let b=b.lock();if b.link==call.from {
                    if let Some(context)=&b.context { context.send_ready(); }
                }}
            }
            cx.tick();return RET_CONTINUE as i32;
        }
        if call.component == COMPONENT && call.method == TICK && call.flags & CALL_LINK == 0 {
            cx.alarm = -1;
            cx.tick();
            return RET_CONTINUE as i32;
        }
        // a raw frame of the module's port is not a request (the runtime parses envelopes and delivers them as requests)
        if call.flags & CALL_LINK != 0 || !call.wants_response() {
            return RET_CONTINUE as i32;
        }
        let method = call.method;
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
        } else if method == 3 {
            match cx.configure_stats(call.data) {
                Ok(()) => cx.write_stats(&mut w),
                Err(rc) => { w.int(rc as i64); },
            }
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
        // an error is a bare negative integer: the runtime makes it `7: error` for an envelope
        hw.reply(call, w.finish().unwrap());
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

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

#[cfg(feature = "sim")]
export!();
