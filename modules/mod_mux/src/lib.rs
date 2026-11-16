//! `mod_mux`: standalone `quic-mux` stream multiplexer endpoint over PPP serial (`--port 245`)
//! and TCP sockets (`--stream 4`, TCP port `5229`).

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod links;
mod pool;
mod streams;

use alloc::vec::Vec;
use links::{MuxLinks, OWNER_ASSOC};
use pool::{XipPacket, XipPool};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, PoolPacket, QuicMux,
    RxMeta, RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId,
};
use streams::StreamSession;
use xip_abi::{
    cbor::Writer, CallView, Hw, Module, Packet, Stream, ALARM_EXACT, CALL_LINK, CALL_LINK_EVENT,
    CALL_STREAM, ERR_ARG, ERR_BUSY, ERR_IO, ERR_NOMEM, ERR_UNSUPPORTED, HOST_START, HOST_STOP,
    RET_CONTINUE, SOCKET_EVENT_CLOSED, SOCKET_EVENT_EOF, SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE,
    SOCKET_EVENT_WRITABLE, SOCKET_EVENT_WRITE_DONE,
};

pub const COMPONENT: i32 = 52;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;

const MAX_ASSOCIATIONS: usize = 4;
const MAX_SESSIONS: usize = 16;
const ACTIVE_POLL_MS: u32 = 10;

fn embedded_mux_limits() -> MuxLimits {
    MuxLimits {
        initial_max_data: 32 * 1024,
        initial_max_stream_data: 16 * 1024,
        initial_max_streams_bidi: 8,
        initial_max_streams_uni: 8,
        max_concurrent_streams: 8,
        max_queued_chunks_per_stream: 4,
        max_association_rx_packets: 8,
        max_device_rx_packets: 12,
        gap_timeout_us: 3_000_000,
    }
}

struct CallbackEnv<'a> {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    sessions: &'a mut Vec<StreamSession>,
    broken_assocs: &'a mut Vec<AssociationId>,
    errors: &'a mut u64,
    last_error: &'a mut i32,
}

impl MuxCallbacks<XipPacket> for CallbackEnv<'_> {
    fn on_stream_data(
        &mut self,
        assoc: AssociationId,
        stream_id: StreamId,
        _offset: u64,
        mut data: RxStreamChunk<'_, XipPacket>,
        fin: bool,
    ) -> Result<RxOutcome<XipPacket>, StreamError> {
        let Some(table) = self.stream_table else {
            *self.errors += 1;
            *self.last_error = ERR_UNSUPPORTED;
            return Err(StreamError { error_code: 1 });
        };
        let pos = match self
            .sessions
            .iter()
            .position(|s| !s.closed && s.assoc == assoc && s.stream_id == stream_id)
        {
            Some(i) => i,
            None => {
                if self.sessions.len() >= MAX_SESSIONS {
                    *self.errors += 1;
                    *self.last_error = ERR_BUSY;
                    return Err(StreamError { error_code: 2 });
                }
                if self.sessions.try_reserve(1).is_err() {
                    *self.errors += 1;
                    *self.last_error = ERR_NOMEM;
                    return Err(StreamError { error_code: 3 });
                }
                self.sessions.push(StreamSession::new(assoc, stream_id));
                self.sessions.len() - 1
            }
        };

        let len = data.len();
        let consumed = match self.sessions[pos].feed(self.hw, table, data.bytes(), fin) {
            Ok(n) => n,
            Err(rc) => {
                *self.errors += 1;
                *self.last_error = rc;
                self.sessions[pos].close_native(self.hw, Some(table));
                return Err(StreamError {
                    error_code: rc.unsigned_abs() as u64,
                });
            }
        };

        if consumed < len {
            if let Some(pkt) = data.take_packet() {
                return Ok(RxOutcome::Partial {
                    consumed,
                    packet: pkt,
                });
            }
        }
        Ok(RxOutcome::Consumed(consumed))
    }

    fn on_stream_writable(
        &mut self,
        _assoc: AssociationId,
        _stream_id: StreamId,
        _available_bytes: usize,
    ) {
    }

    fn on_association_writable(&mut self, _assoc: AssociationId, _available_bytes: u64) {}

    fn on_device_writable(&mut self) {}

    fn on_stream_closed(
        &mut self,
        assoc: AssociationId,
        stream_id: StreamId,
        _reason: StreamCloseReason,
    ) {
        if let Some(s) = self
            .sessions
            .iter_mut()
            .find(|s| !s.closed && s.assoc == assoc && s.stream_id == stream_id)
        {
            s.close_native(self.hw, self.stream_table);
        }
    }

    fn on_association_broken(&mut self, assoc: AssociationId, _reason: AssociationBrokenReason) {
        if !self.broken_assocs.contains(&assoc) {
            self.broken_assocs.push(assoc);
        }
    }
}

struct Context {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    mux: QuicMux<XipPool>,
    links: MuxLinks,
    associations: Vec<AssociationId>,
    sessions: Vec<StreamSession>,
    alarm: i32,
    received_frames: u64,
    errors: u64,
    last_error: i32,
}

impl Context {
    #[inline(never)]
    fn new(hw: Hw) -> Self {
        Self {
            hw,
            stream_table: hw.stream(),
            mux: QuicMux::new(XipPool::new(hw), embedded_mux_limits()),
            links: MuxLinks::new(hw),
            associations: Vec::new(),
            sessions: Vec::new(),
            alarm: -1,
            received_frames: 0,
            errors: 0,
            last_error: 0,
        }
    }

    fn now_us(&self, fallback_us: u32) -> u64 {
        self.hw.now_us().unwrap_or_else(|| {
            let ms = self.hw.now_ms();
            if ms != 0 {
                ms.saturating_mul(1000)
            } else {
                u64::from(fallback_us)
            }
        })
    }

    fn ensure_association(&mut self, assoc: AssociationId) -> Result<(), i32> {
        if assoc.0 == 0 {
            return Err(ERR_ARG);
        }
        if self.associations.contains(&assoc) {
            return Ok(());
        }
        if self.associations.len() >= MAX_ASSOCIATIONS {
            return Err(ERR_BUSY);
        }
        self.mux
            .add_association(assoc, false)
            .map_err(|_| ERR_ARG)?;
        self.associations.push(assoc);
        Ok(())
    }

    fn drop_association(&mut self, assoc: AssociationId) {
        let mut broken = Vec::new();
        {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.remove_association(assoc, &mut env);
        }
        for s in &mut self.sessions {
            if s.assoc == assoc && !s.closed {
                s.close_native(self.hw, self.stream_table);
            }
        }
        self.sessions.retain(|s| !s.closed);
        self.associations.retain(|&a| a != assoc);
    }

    fn receive_serial(&mut self, call: &CallView, packet: Packet) -> Result<(), i32> {
        if call.data.is_empty() {
            return Err(ERR_ARG);
        }
        let assoc = OWNER_ASSOC;
        let handle = packet.into_raw();
        let pkt = XipPacket::from_owned(self.hw, handle, 0..0)?;
        let base = pkt.slot_slice().as_ptr() as usize;
        let start = (call.data.as_ptr() as usize)
            .checked_sub(base)
            .ok_or(ERR_ARG)?;
        let mut pkt = pkt;
        pkt.set_range(start..(start + call.data.len()))
            .map_err(|_| ERR_ARG)?;

        self.links.note_serial_peer(assoc, call.from, call.id);
        self.ensure_association(assoc)?;
        self.received_frames += 1;

        let now_us = self.now_us(call.time_us);
        self.ingest_mux_frame(assoc, pkt, now_us)
    }

    fn ingest_mux_frame(
        &mut self,
        assoc: AssociationId,
        pkt: XipPacket,
        now_us: u64,
    ) -> Result<(), i32> {
        let mut broken = Vec::new();
        let res = {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.receive_packet(
                RxMeta {
                    association: assoc,
                    received_at_us: now_us,
                },
                pkt,
                &mut self.links,
                &mut env,
            )
        };
        for b in broken {
            self.drop_association(b);
        }
        res.map_err(|_| ERR_IO)
    }

    fn handle_socket_event(&mut self, socket: u32, event: u32) {
        match event {
            SOCKET_EVENT_OPEN => {
                self.links.add_tcp_socket(socket);
                self.read_tcp_frames(socket);
            }
            SOCKET_EVENT_READABLE => {
                self.read_tcp_frames(socket);
            }
            SOCKET_EVENT_WRITABLE => {
                if let Some(assoc) = self.links.on_tcp_writable(socket) {
                    self.notify_assoc_ready(assoc);
                }
            }
            SOCKET_EVENT_WRITE_DONE => {
                if let Some(assoc) = self.links.on_tcp_write_done(socket) {
                    self.notify_assoc_ready(assoc);
                }
                let should_close = self
                    .links
                    .tcp
                    .iter()
                    .find(|t| t.socket == socket)
                    .is_some_and(|t| t.peer_eof && t.can_accept_tx());
                if should_close {
                    if let Some(assoc) = self.links.remove_tcp_socket(socket) {
                        if !self.links.has_any_bearer(assoc) {
                            self.drop_association(assoc);
                        }
                    }
                }
            }
            SOCKET_EVENT_EOF | SOCKET_EVENT_CLOSED => {
                if let Some(assoc) = self.links.remove_tcp_socket(socket) {
                    if !self.links.has_any_bearer(assoc) {
                        self.drop_association(assoc);
                    }
                }
            }
            _ => {}
        }
    }

    fn notify_assoc_ready(&mut self, assoc: AssociationId) {
        let mut broken = Vec::new();
        {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux
                .on_association_ready(assoc, &mut self.links, &mut env);
        }
        for b in broken {
            self.drop_association(b);
        }
    }

    fn read_tcp_frames(&mut self, socket: u32) {
        let pool = *self.mux.pool();
        let now_us = self.now_us(0);
        let mut frames: Vec<(AssociationId, XipPacket)> = Vec::new();
        let res = self.links.drain_tcp_reads(&pool, socket, |_links, assoc, pkt| {
            frames.push((assoc, pkt));
            Ok(())
        });
        for (assoc, pkt) in frames {
            if let Err(rc) = self.ensure_association(assoc) {
                self.errors += 1;
                self.last_error = rc;
                continue;
            }
            self.received_frames += 1;
            if let Err(rc) = self.ingest_mux_frame(assoc, pkt, now_us) {
                self.errors += 1;
                self.last_error = rc;
            }
        }
        match res {
            Ok(true) => {
                if let Some(assoc) = self.links.remove_tcp_socket(socket) {
                    if !self.links.has_any_bearer(assoc) {
                        self.drop_association(assoc);
                    }
                }
            }
            Ok(false) => {}
            Err(rc) => {
                self.errors += 1;
                self.last_error = rc;
                if let Some(assoc) = self.links.remove_tcp_socket(socket) {
                    if !self.links.has_any_bearer(assoc) {
                        self.drop_association(assoc);
                    }
                }
            }
        }
    }

    #[inline(never)]
    fn progress(&mut self, now_us: u64) {
        // 1. Advance gap timeouts.
        let mut broken = Vec::new();
        {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.advance_time(now_us, &mut self.links, &mut env);
        }
        for b in broken {
            self.drop_association(b);
        }

        // 2. Retry streams paused by partial consumer `feed`.
        let paused: Vec<(AssociationId, StreamId)> = self
            .sessions
            .iter()
            .filter(|s| !s.closed && s.needs_resume)
            .map(|s| (s.assoc, s.stream_id))
            .collect();
        for (assoc, sid) in paused {
            let mut broken = Vec::new();
            let res = {
                let mut env = CallbackEnv {
                    hw: self.hw,
                    stream_table: self.stream_table,
                    sessions: &mut self.sessions,
                    broken_assocs: &mut broken,
                    errors: &mut self.errors,
                    last_error: &mut self.last_error,
                };
                self.mux
                    .resume_stream(assoc, sid, now_us, &mut self.links, &mut env)
            };
            if res.is_err() {
                self.errors += 1;
                self.last_error = ERR_IO;
            }
            for b in broken {
                self.drop_association(b);
            }
        }

        // 3. Pump consumer stream egress (`c_stream_take` -> zero-copy `QuicMux::write_stream`).
        if let Some(table) = self.stream_table {
            for s in &mut self.sessions {
                if !s.closed {
                    if let Err(rc) = s.pump_egress(self.hw, table, &mut self.mux, &mut self.links) {
                        self.errors += 1;
                        self.last_error = rc;
                    }
                }
            }
        }

        // 4. Flush standalone flow-control updates if needed.
        for &assoc in &self.associations {
            let _ = self.mux.flush_control(assoc, &mut self.links);
        }

        self.sessions.retain(|s| !s.closed);
    }

    fn update_alarm(&mut self, now_us: u64) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        let has_active_sessions = self.sessions.iter().any(|s| !s.closed);
        let gap_deadline = self.mux.next_deadline();
        if !has_active_sessions && gap_deadline.is_none() {
            return;
        }
        let delay_ms = match (has_active_sessions, gap_deadline) {
            (true, Some(dl)) => {
                let gap_ms = dl.saturating_sub(now_us).div_ceil(1000).max(1) as u32;
                gap_ms.min(ACTIVE_POLL_MS)
            }
            (true, None) => ACTIVE_POLL_MS,
            (false, Some(dl)) => dl.saturating_sub(now_us).div_ceil(1000).max(1) as u32,
            (false, None) => return,
        };
        self.alarm = self
            .hw
            .alarm_set(delay_ms, 0, 0, ALARM_EXACT, COMPONENT, TICK);
    }

    fn tick(&mut self, fallback_us: u32) {
        let now_us = self.now_us(fallback_us);
        self.progress(now_us);
        self.update_alarm(now_us);
    }

    fn teardown(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        for s in &mut self.sessions {
            s.close_native(self.hw, self.stream_table);
        }
        self.sessions.clear();
        let assocs = core::mem::take(&mut self.associations);
        let mut broken = Vec::new();
        for assoc in assocs {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.remove_association(assoc, &mut env);
        }
        self.links.close_all();
    }

    fn reset(&mut self) {
        self.teardown();
        self.mux = QuicMux::new(XipPool::new(self.hw), embedded_mux_limits());
    }
}

#[derive(Default)]
pub struct State {
    context: usize,
}

impl State {
    fn start(&mut self, hw: &Hw) -> Result<(), i32> {
        if self.context != 0 {
            return Err(ERR_BUSY);
        }
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Context>())
            .cast::<Context>();
        if ptr.is_null() {
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Context::new(*hw));
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
        unsafe {
            (*ptr).teardown();
            ptr.drop_in_place();
        }
        hw.mem_free(ptr.cast());
    }
}

impl Module for State {
    fn handle_owned(&mut self, hw: &Hw, call: &CallView, packet: Option<Packet>) -> i32 {
        if self.context != 0
            && call.component == COMPONENT
            && call.method == 0
            && call.flags & CALL_LINK != 0
            && call.flags & CALL_LINK_EVENT == 0
        {
            if let Some(pkt) = packet {
                let cx = unsafe { &mut *(self.context as *mut Context) };
                if let Err(rc) = cx.receive_serial(call, pkt) {
                    cx.errors += 1;
                    cx.last_error = rc;
                }
                cx.tick(call.time_us);
                return RET_CONTINUE as i32;
            }
        }
        self.handle(hw, call)
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
            let assocs = cx.associations.clone();
            for assoc in assocs {
                cx.notify_assoc_ready(assoc);
            }
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        if call.component == COMPONENT && call.method == TICK && call.flags & CALL_LINK == 0 {
            if cx.alarm >= 0 {
                cx.hw.alarm_cancel(cx.alarm);
                cx.alarm = -1;
            }
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        // Socket readiness / lifecycle events on `--stream 4`.
        if call.component == COMPONENT
            && call.method == 0
            && call.flags & (CALL_LINK | CALL_STREAM) == 0
            && !call.wants_response()
        {
            cx.handle_socket_event(call.id as u32, call.value as u32);
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        if call.flags & CALL_LINK != 0 || !call.wants_response() {
            return RET_CONTINUE as i32;
        }

        let mut out = [0u8; 160];
        let mut w = Writer::new(&mut out);
        match call.method {
            1 => {
                let active_streams = cx.sessions.iter().filter(|s| !s.closed).count() as u64;
                w.map(8)
                    .uint(1)
                    .uint(cx.associations.len() as u64)
                    .uint(2)
                    .uint(active_streams)
                    .uint(3)
                    .uint(cx.links.tcp_count())
                    .uint(4)
                    .uint(cx.received_frames)
                    .uint(5)
                    .uint(cx.links.sent_frames)
                    .uint(6)
                    .uint(cx.links.coalesced_packets)
                    .uint(7)
                    .uint(cx.errors)
                    .uint(8)
                    .int(cx.last_error as i64);
            }
            2 => {
                cx.reset();
                w.map(0);
            }
            _ => {
                w.int(ERR_UNSUPPORTED as i64);
            }
        }
        if let Some(reply) = w.finish() {
            hw.reply(call, reply);
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

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(feature = "sim")]
export!();
