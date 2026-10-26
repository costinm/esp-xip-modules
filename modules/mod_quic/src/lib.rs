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
                max_routes: 8, // Local association/path entries; no relay resolver is installed.
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
    fn advance_clock(&mut self, now: u64)->Result<(),i32> {
        match self.runtime.advance_time(now){Ok(_)=>Ok(()),Err(e) if e.is_retryable()=>Ok(()),Err(_)=>Err(ERR_IO)}
    }
    #[inline(never)]
    fn progress_ingress(&mut self)->Result<(),i32> {
        for _ in 0..4 {
            match self.runtime.progress() {
                Ok(true) => {}
                Ok(false)=>break,
                Err(e) if e.is_retryable()=>break,
                Err(_)=>return Err(ERR_IO),
            }
        }
        Ok(())
    }
    fn progress(&mut self, now: u64) -> Result<(), i32> {
        self.poll_completions()?;
        self.advance_clock(now)?;
        self.progress_ingress()?;
        while self.runtime.next_association().is_some() {}
        let table = self.streams.ok_or(ERR_UNSUPPORTED)?;
        // The portable owned-chunk API bounds admission to one queued chunk per
        // stream. Retain/move that allocation until its consumer accepts it;
        // no read/copy through lwIP or a second body queue.
        if self.sessions.iter().all(|s| s.input.is_none()) {
            if let Some(chunk) = self.runtime.next_stream_chunk() {
                let at = if let Some(i) = self
                    .sessions
                    .iter()
                    .position(|s| s.stream.same_stream(&chunk.stream))
                {
                    i
                } else {
                    if self.sessions.len() >= 2 {
                        return Err(ERR_BUSY);
                    }
                    self.sessions.try_reserve(1).map_err(|_| ERR_NOMEM)?;
                    self.sessions.push(Session::new(chunk.stream));
                    self.sessions.len() - 1
                };
                self.sessions[at].input = Some(streams::Input {
                    bytes: chunk.bytes,
                    fin: chunk.fin,
                });
            }
        }
        for s in &mut self.sessions {
            if s.closed {
                continue;
            }
            if let Err(rc) = s.feed(self.pool.native, table) {
                s.close(self.pool.native, table);
                let _ = self.runtime.finish_stream(&mut s.stream);
                return Err(rc);
            }
            if s.handle != 0 && s.output.is_none() && self.pool.native.info()?.available > 2 {
                let mut out = StreamPacket::default();
                let rc = unsafe {
                    table.take.ok_or(ERR_UNSUPPORTED)?(
                        self.pool.native.hw.ctx(),
                        s.handle,
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
                    // Split immutable ranges of the SAME pool lease. Reliable
                    // history owns each accepted range; no payload clone.
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
                                s.handle,
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
                if let Some(t) = c.streams {
                    for s in &mut c.sessions {
                        s.close(c.pool.native, t);
                    }
                }
                c.sessions.clear();
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
