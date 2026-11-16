//! `mod_quic2`: combined `quic-link` (`QuicLinkAssoc`) + `quic-mux` (`QuicMux`) multi-link
//! endpoint over UDP (`hw.quic` Short-Header classifier and datagram port `243`), PPP serial
//! (`--port 243`), and TCP sockets (`--stream 6`, TCP port `5231`).

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod links;
mod pool;
mod streams;

use alloc::vec::Vec;
use links::{
    serial_link_profile, udp_link_profile, PeerRoute, Quic2Drivers, LINK_SERIAL, LINK_UDP,
    OWNER_ASSOC, QUIC2_SERIAL_PORT,
};
use pool::{XipPacket, XipPool};
use quic_link::{AssocConfig, ConnectionId, LinkId, QuicLinkAssoc};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, PoolPacket, QuicMux, RxMeta,
    RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId,
};
use streams::StreamSession;
use xip_abi::{
    cbor::Writer, CallView, Hw, LinkInfo, Module, Packet, Quic, Stream, ALARM_EXACT, CALL_LINK,
    CALL_LINK_EVENT, CALL_STREAM, ERR_ARG, ERR_BUSY, ERR_IO, ERR_NOMEM, ERR_UNSUPPORTED,
    HOST_START, HOST_STOP, LINK_NATIVE, QUIC_VERSION, RET_CONTINUE, SOCKET_EVENT_CLOSED,
    SOCKET_EVENT_EOF, SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE, SOCKET_EVENT_WRITABLE,
    SOCKET_EVENT_WRITE_DONE,
};

pub const COMPONENT: i32 = 55;
const RX: i32 = 100;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;

const MAX_SESSIONS: usize = 16;
const ACTIVE_POLL_MS: u32 = 10;

fn embedded_mux_limits() -> MuxLimits {
    MuxLimits {
        initial_max_data: 64 * 1024,
        initial_max_stream_data: 32 * 1024,
        initial_max_streams_bidi: 8,
        initial_max_streams_uni: 8,
        max_concurrent_streams: 8,
        max_queued_chunks_per_stream: 6,
        max_association_rx_packets: 8,
        max_device_rx_packets: 10,
        gap_timeout_us: 3_000_000,
    }
}

fn default_assoc_config() -> AssocConfig {
    let default_cid = ConnectionId::new(1).unwrap();
    AssocConfig {
        assoc_id: OWNER_ASSOC,
        local_cid: None,
        default_peer_cid: default_cid,
        max_retained_tx_packets: 6,
        ack_eliciting_threshold: 2,
        max_ack_delay_us: 20_000,
        ..AssocConfig::default()
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
    quic_attached: bool,
    mux: QuicMux<XipPool>,
    assoc: QuicLinkAssoc<XipPacket>,
    drivers: Quic2Drivers,
    has_owner_assoc: bool,
    sessions: Vec<StreamSession>,
    alarm: i32,
    received_packets: u64,
    errors: u64,
    last_error: i32,
}

impl Context {
    #[inline(never)]
    fn new(hw: Hw) -> Self {
        let mut assoc = QuicLinkAssoc::new(default_assoc_config());
        assoc.upsert_link(LINK_UDP, udp_link_profile());
        assoc.upsert_link(LINK_SERIAL, serial_link_profile());
        if let Some(l) = assoc.link_mut(LINK_UDP) {
            l.enabled = false;
        }
        if let Some(l) = assoc.link_mut(LINK_SERIAL) {
            l.enabled = false;
        }
        let mut cx = Self {
            hw,
            stream_table: hw.stream(),
            quic_attached: false,
            mux: QuicMux::new(XipPool::new(hw), embedded_mux_limits()),
            assoc,
            drivers: Quic2Drivers::new(hw),
            has_owner_assoc: false,
            sessions: Vec::new(),
            alarm: -1,
            received_packets: 0,
            errors: 0,
            last_error: 0,
        };
        cx.ensure_quic_attached();
        cx
    }

    fn ensure_quic_attached(&mut self) {
        if self.quic_attached {
            return;
        }
        let rc = Self::try_attach_quic(self.hw);
        if rc == 0 {
            self.quic_attached = true;
            if self.last_error < 0 {
                self.last_error = 0;
            }
        } else {
            if rc == ERR_BUSY {
                let _ = self.hw.module_stop(b"quic");
            }
            self.last_error = rc;
            if self.alarm < 0 {
                self.alarm = self
                    .hw
                    .alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            }
        }
    }

    fn try_attach_quic(hw: Hw) -> i32 {
        let p = hw.get_component(c"quic") as *const Quic;
        if p.is_null() {
            return -101;
        }
        let q = unsafe { &*p };
        if q.version != QUIC_VERSION || (q.size as usize) < core::mem::size_of::<Quic>() {
            return -102;
        }
        let Some(attach) = q.attach else {
            return -103;
        };
        unsafe { attach(hw.ctx(), RX) }
    }

    fn detach_quic(&mut self) {
        if !self.quic_attached {
            return;
        }
        self.quic_attached = false;
        let p = self.hw.get_component(c"quic") as *const Quic;
        if p.is_null() {
            return;
        }
        let q = unsafe { &*p };
        if let Some(detach) = q.detach {
            unsafe {
                detach(self.hw.ctx());
            }
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

    fn ensure_owner_assoc(&mut self) -> Result<(), i32> {
        if self.has_owner_assoc {
            return Ok(());
        }
        self.mux
            .add_association(OWNER_ASSOC, false)
            .map_err(|_| ERR_ARG)?;
        self.has_owner_assoc = true;
        Ok(())
    }

    fn reinit_owner_assoc(&mut self) {
        let mut broken = Vec::new();
        if self.has_owner_assoc {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.remove_association(OWNER_ASSOC, &mut env);
            self.has_owner_assoc = false;
        }
        for s in &mut self.sessions {
            if !s.closed {
                s.close_native(self.hw, self.stream_table);
            }
        }
        self.sessions.clear();
    }

    fn is_serial_link(&self, link_addr: u32, call_method: i32) -> bool {
        if call_method == RX {
            return false;
        }
        if let Some(link_table) = self.hw.link() {
            if let Some(info_fn) = link_table.info {
                let mut info: LinkInfo = unsafe { core::mem::zeroed() };
                info.size = core::mem::size_of::<LinkInfo>() as u32;
                if unsafe { info_fn(self.hw.ctx(), link_addr, &mut info) } == 0
                    && info.provider == LINK_NATIVE
                {
                    let len = info
                        .name
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(info.name.len());
                    let name = &info.name[..len];
                    if name == b"udp" || name == b"udp6" || name.starts_with(b"udp:") {
                        return false;
                    }
                    if name == b"serial" || name == b"ppp" {
                        return true;
                    }
                }
            }
        }
        true
    }

    fn receive_link_packet(&mut self, call: &CallView, packet: Packet) -> Result<(), i32> {
        if call.data.is_empty() {
            return Err(ERR_ARG);
        }
        let handle = packet.into_raw();
        let pkt = XipPacket::from_owned(self.hw, handle, 0..0)?;
        let base = pkt.slot_slice().as_ptr() as usize;
        let start = (call.data.as_ptr() as usize)
            .checked_sub(base)
            .ok_or(ERR_ARG)?;
        let mut pkt = pkt;
        pkt.set_range(start..(start + call.data.len()))
            .map_err(|_| ERR_ARG)?;

        let is_serial = self.is_serial_link(call.from, call.method);
        let link_id = if is_serial {
            let token = if call.id == 0 {
                QUIC2_SERIAL_PORT
            } else {
                call.id
            };
            self.drivers.serial_peer = PeerRoute {
                active: true,
                link: call.from,
                token,
            };
            if let Some(l) = self.assoc.link_mut(LINK_SERIAL) {
                l.enabled = true;
                l.driver_ready = true;
            }
            LINK_SERIAL
        } else {
            self.drivers.udp_peer = PeerRoute {
                active: true,
                link: call.from,
                token: call.id,
            };
            if let Some(l) = self.assoc.link_mut(LINK_UDP) {
                l.enabled = true;
                l.driver_ready = true;
            }
            LINK_UDP
        };

        let now_us = self.now_us(call.time_us);
        self.ingest_quic2_packet(link_id, pkt, now_us)
    }

    fn ingest_quic2_packet(
        &mut self,
        link_id: LinkId,
        pkt: XipPacket,
        now_us: u64,
    ) -> Result<(), i32> {
        self.ensure_owner_assoc()?;
        self.received_packets += 1;
        let outcome = self
            .assoc
            .receive_packet(link_id, pkt, now_us)
            .map_err(|_| ERR_IO)?;

        if let Some(mux_pkt) = outcome.mux_packet {
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
                        association: OWNER_ASSOC,
                        received_at_us: now_us,
                    },
                    mux_pkt,
                    &mut self.assoc,
                    &mut env,
                )
            };
            if !broken.is_empty() {
                self.reinit_owner_assoc();
            }
            res.map_err(|_| ERR_IO)?;
        }
        Ok(())
    }

    fn handle_socket_event(&mut self, socket: u32, event: u32, now_us: u64) {
        match event {
            SOCKET_EVENT_OPEN => {
                if self.drivers.add_tcp_socket(socket, &mut self.assoc).is_some() {
                    self.read_tcp_frames(socket, now_us);
                }
            }
            SOCKET_EVENT_READABLE => {
                self.read_tcp_frames(socket, now_us);
            }
            SOCKET_EVENT_WRITABLE => {
                self.drivers.on_tcp_writable(socket, &mut self.assoc, now_us);
                self.notify_assoc_ready();
            }
            SOCKET_EVENT_WRITE_DONE => {
                self.drivers
                    .on_tcp_write_done(socket, &mut self.assoc, now_us);
                self.notify_assoc_ready();
                let should_close = self
                    .drivers
                    .tcp
                    .iter()
                    .find(|t| t.socket == socket)
                    .is_some_and(|t| t.peer_eof && t.is_tx_idle());
                if should_close {
                    self.drivers
                        .remove_tcp_socket(socket, &mut self.assoc, now_us);
                }
            }
            SOCKET_EVENT_EOF | SOCKET_EVENT_CLOSED => {
                self.drivers
                    .remove_tcp_socket(socket, &mut self.assoc, now_us);
            }
            _ => {}
        }
    }

    fn notify_assoc_ready(&mut self) {
        if !self.has_owner_assoc {
            return;
        }
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
                .on_association_ready(OWNER_ASSOC, &mut self.assoc, &mut env);
        }
        if !broken.is_empty() {
            self.reinit_owner_assoc();
        }
    }

    fn read_tcp_frames(&mut self, socket: u32, now_us: u64) {
        let pool = *self.mux.pool();
        let mut packets: Vec<(LinkId, XipPacket)> = Vec::new();
        let res = self.drivers.drain_tcp_reads(&pool, socket, |lid, pkt| {
            packets.push((lid, pkt));
            Ok(())
        });
        for (lid, pkt) in packets {
            if let Err(rc) = self.ingest_quic2_packet(lid, pkt, now_us) {
                self.errors += 1;
                self.last_error = rc;
            }
        }
        match res {
            Ok(true) => {
                self.drivers
                    .remove_tcp_socket(socket, &mut self.assoc, now_us);
            }
            Ok(false) => {}
            Err(rc) => {
                self.errors += 1;
                self.last_error = rc;
                self.drivers
                    .remove_tcp_socket(socket, &mut self.assoc, now_us);
            }
        }
    }

    #[inline(never)]
    fn progress(&mut self, now_us: u64) {
        self.assoc.set_now_us(now_us);

        // 1. Advance `QuicMux` gap timeouts.
        if self.has_owner_assoc {
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
                self.mux.advance_time(now_us, &mut self.assoc, &mut env);
            }
            if !broken.is_empty() {
                self.reinit_owner_assoc();
            }
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
                    .resume_stream(assoc, sid, now_us, &mut self.assoc, &mut env)
            };
            if res.is_err() {
                self.errors += 1;
                self.last_error = ERR_IO;
            }
            if !broken.is_empty() {
                self.reinit_owner_assoc();
            }
        }

        // 3. Pump consumer stream egress (`c_stream_take` -> zero-copy `QuicMux::write_stream` -> `QuicLinkAssoc`).
        if let Some(table) = self.stream_table {
            for s in &mut self.sessions {
                if !s.closed {
                    if let Err(rc) = s.pump_egress(self.hw, table, &mut self.mux, &mut self.assoc) {
                        self.errors += 1;
                        self.last_error = rc;
                    }
                }
            }
        }

        // 4. Flush standalone `QuicMux` flow-control updates (`MAX_DATA`, `MAX_STREAM_DATA`, `MAX_STREAMS`).
        if self.has_owner_assoc {
            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
        }

        // 5. Poll `QuicLinkAssoc` timers (delayed `ACK`, SACK loss detection, PTO).
        let mut pool = *self.mux.pool();
        let _ = self.assoc.poll_timers(&mut pool, now_us);

        // 6. Reset any streams evicted from the SACK ledger under memory pressure.
        for (assoc, sid) in self.assoc.take_evicted_streams() {
            let _ = self.mux.reset_stream(assoc, sid, 1, &mut self.assoc);
            if let Some(s) = self
                .sessions
                .iter_mut()
                .find(|s| !s.closed && s.assoc == assoc && s.stream_id == sid)
            {
                s.close_native(self.hw, self.stream_table);
            }
        }

        // 7. Flush framed outbound packets from `QuicLinkAssoc` onto TCP, UDP, or PPP serial.
        self.drivers.flush_outbound(&mut self.assoc, now_us);

        self.sessions.retain(|s| !s.closed);
    }

    fn update_alarm(&mut self, now_us: u64) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        let has_active_sessions = self.sessions.iter().any(|s| !s.closed);
        let mux_dl = self.mux.next_deadline();
        let assoc_dl = self.assoc.next_deadline_us();
        let earliest_dl = match (mux_dl, assoc_dl) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        if !self.quic_attached {
            self.alarm = self
                .hw
                .alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            return;
        }
        if !has_active_sessions && earliest_dl.is_none() {
            return;
        }
        let delay_ms = match (has_active_sessions, earliest_dl) {
            (true, Some(dl)) => {
                let dl_ms = dl.saturating_sub(now_us).div_ceil(1000).max(1) as u32;
                dl_ms.min(ACTIVE_POLL_MS)
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
        self.ensure_quic_attached();
        let now_us = self.now_us(fallback_us);
        self.progress(now_us);
        self.update_alarm(now_us);
    }

    fn teardown(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        self.reinit_owner_assoc();
        self.drivers.close_all();
        self.detach_quic();
    }

    fn reset(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        self.reinit_owner_assoc();
        self.drivers.close_all();
        self.mux = QuicMux::new(XipPool::new(self.hw), embedded_mux_limits());
        self.assoc = QuicLinkAssoc::new(default_assoc_config());
        self.assoc.upsert_link(LINK_UDP, udp_link_profile());
        self.assoc.upsert_link(LINK_SERIAL, serial_link_profile());
        if let Some(l) = self.assoc.link_mut(LINK_UDP) {
            l.enabled = false;
        }
        if let Some(l) = self.assoc.link_mut(LINK_SERIAL) {
            l.enabled = false;
        }
        self.ensure_quic_attached();
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
            && (call.method == 0 || call.method == RX)
            && call.flags & CALL_LINK != 0
            && call.flags & CALL_LINK_EVENT == 0
        {
            if let Some(pkt) = packet {
                let cx = unsafe { &mut *(self.context as *mut Context) };
                if let Err(rc) = cx.receive_link_packet(call, pkt) {
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
            cx.notify_assoc_ready();
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

        // Socket readiness / lifecycle events on `--stream 6`.
        if call.component == COMPONENT
            && call.method == 0
            && call.flags & (CALL_LINK | CALL_STREAM) == 0
            && !call.wants_response()
        {
            let now_us = cx.now_us(call.time_us);
            cx.handle_socket_event(call.id as u32, call.value as u32, now_us);
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
                cx.ensure_quic_attached();
                let active_streams = cx.sessions.iter().filter(|s| !s.closed).count() as u64;
                let retained = cx.assoc.sack_ledger().retained_packets() as u64;
                w.map(8)
                    .uint(1)
                    .uint(u64::from(cx.has_owner_assoc))
                    .uint(2)
                    .uint(active_streams)
                    .uint(3)
                    .uint(cx.drivers.tcp_count())
                    .uint(4)
                    .uint(cx.received_packets)
                    .uint(5)
                    .uint(cx.drivers.sent_packets)
                    .uint(6)
                    .uint(retained)
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
