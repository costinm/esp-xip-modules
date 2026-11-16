//! `mod_qlink`: standalone single-stream `quic-link` endpoint (`SingleStreamEndpoint`) over UDP
//! (`hw.quic` Short-Header classifier and datagram port `242`) and PPP serial (`--port 242`).

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod pool;

use pool::{XipPacket, XipPool};
use quic_link::{
    flow::{RxOutcome, StreamPriority},
    link::OWNER_ASSOC,
    pool::{PacketPool, PoolPacket},
    streams::{RxStreamChunk, TxStreamData},
    AssocConfig, CongestionMode, ConnectionId, LinkFraming, LinkId, LinkProfile,
    SingleStreamCallbacks, SingleStreamConfig, SingleStreamEndpoint, StreamId,
};
use xip_abi::{
    cbor::Writer, CallView, Hw, LinkInfo, Module, Packet, Quic, Stream, StreamPacket, ALARM_EXACT,
    CALL_LINK, CALL_LINK_EVENT, ERR_ARG, ERR_BUSY, ERR_IO, ERR_NOMEM, ERR_NOTFOUND,
    ERR_UNSUPPORTED, HOST_START, HOST_STOP, LINK_NATIVE, PKT_HEADROOM, PKT_MAX, QUIC_VERSION,
    RET_CONTINUE,
};

pub const COMPONENT: i32 = 54;
pub const QLINK_SERIAL_PORT: u64 = 242;
const RX: i32 = 100;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;

const LINK_UDP: LinkId = LinkId(1);
const LINK_SERIAL: LinkId = LinkId(2);
const ACTIVE_POLL_MS: u32 = 10;

fn default_qlink_config() -> SingleStreamConfig {
    let default_cid = ConnectionId::new(1).unwrap();
    SingleStreamConfig {
        stream_id: StreamId(0),
        stream_priority: StreamPriority::NORMAL,
        initial_recv_window: 64 * 1024,
        initial_send_window: 64 * 1024,
        max_reassembly_chunks: 6,
        assoc: AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: None,
            default_peer_cid: default_cid,
            max_retained_tx_packets: 6,
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 20_000,
            ..AssocConfig::default()
        },
    }
}

fn udp_link_profile() -> LinkProfile {
    let cid = ConnectionId::new(1).unwrap();
    LinkProfile {
        tier: 0,
        mtu: 1472,
        max_priority: StreamPriority::BULK,
        congestion_mode: CongestionMode::FlowControlled {
            max_inflight_packets: 4,
            max_inflight_bytes: 6 * 1024,
        },
        framing: LinkFraming::Datagram { peer_cid: cid },
        initial_rtt_us: 25_000,
    }
}

fn serial_link_profile() -> LinkProfile {
    let cid = ConnectionId::new(1).unwrap();
    LinkProfile {
        tier: 1,
        mtu: PKT_MAX as usize,
        max_priority: StreamPriority::BULK,
        congestion_mode: CongestionMode::FlowControlled {
            max_inflight_packets: 4,
            max_inflight_bytes: 5 * 1024,
        },
        framing: LinkFraming::Datagram { peer_cid: cid },
        initial_rtt_us: 150_000,
    }
}

#[derive(Clone, Copy, Default)]
struct PeerRoute {
    active: bool,
    link: u32,
    token: u64,
}

#[derive(Default)]
struct SingleSession {
    handle: u32,
    needs_resume: bool,
    paused_pkt: Option<XipPacket>,
    paused_fin: bool,
    rx_fin: bool,
    tx_fin: bool,
    output: Option<XipPacket>,
    output_fin: bool,
}

impl SingleSession {
    fn is_active(&self) -> bool {
        self.handle != 0
            || self.paused_pkt.is_some()
            || self.output.is_some()
            || (self.rx_fin && !self.tx_fin)
    }

    fn close_native(&mut self, hw: Hw, table: Option<&Stream>) {
        if self.handle != 0 {
            if let Some(close) = table.and_then(|t| t.close) {
                unsafe {
                    close(hw.ctx(), self.handle);
                }
            }
            self.handle = 0;
        }
        self.needs_resume = false;
        self.paused_pkt = None;
        self.paused_fin = false;
        self.rx_fin = false;
        self.tx_fin = false;
        self.output = None;
        self.output_fin = false;
    }

    fn feed(&mut self, hw: Hw, table: &Stream, bytes: &[u8], fin: bool) -> Result<usize, i32> {
        if self.rx_fin {
            return Err(ERR_ARG);
        }
        if self.handle == 0 {
            let rc =
                unsafe { table.open.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), core::ptr::null(), 0) };
            if rc <= 0 {
                return Err(if rc < 0 { rc } else { ERR_IO });
            }
            self.handle = rc as u32;
        }
        let n = if bytes.is_empty() {
            0
        } else {
            let rc = unsafe {
                table.feed.ok_or(ERR_UNSUPPORTED)?(
                    hw.ctx(),
                    self.handle,
                    bytes.as_ptr(),
                    bytes.len(),
                    0,
                )
            };
            if rc < 0 {
                return Err(rc);
            }
            let consumed = rc as usize;
            if consumed > bytes.len() {
                return Err(ERR_ARG);
            }
            consumed
        };
        self.needs_resume = n < bytes.len();
        if !self.needs_resume && fin {
            let rc = unsafe {
                table.feed.ok_or(ERR_UNSUPPORTED)?(
                    hw.ctx(),
                    self.handle,
                    core::ptr::null(),
                    0,
                    1,
                )
            };
            if rc < 0 {
                return Err(rc);
            }
            self.rx_fin = true;
        }
        Ok(n)
    }

    fn pump_egress(
        &mut self,
        hw: Hw,
        table: &Stream,
        endpoint: &mut SingleStreamEndpoint<XipPool>,
        now_us: u64,
    ) -> Result<(), i32> {
        if self.tx_fin || (self.handle == 0 && self.output.is_none()) {
            return Ok(());
        }
        loop {
            if self.output.is_none() && self.handle != 0 {
                if endpoint.pool().available() <= 4 {
                    return Ok(());
                }
                let mut sp = StreamPacket::default();
                let rc = unsafe {
                    table.take.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle, &mut sp)
                };
                if rc >= 0 && sp.packet != 0 {
                    let start = PKT_HEADROOM as usize;
                    let pkt =
                        XipPacket::from_owned(hw, sp.packet, start..start + sp.len as usize)?;
                    self.output = Some(pkt);
                    self.output_fin = sp.fin != 0;
                } else if rc == ERR_NOTFOUND {
                    match endpoint.write_stream(TxStreamData::Bytes(&[]), true, now_us) {
                        Ok(_) => {
                            self.tx_fin = true;
                            if self.rx_fin {
                                self.close_native(hw, Some(table));
                            }
                        }
                        Err(_) => {}
                    }
                    return Ok(());
                } else if rc == 0 || rc == ERR_BUSY {
                    return Ok(());
                } else {
                    self.close_native(hw, Some(table));
                    return Err(rc);
                }
            }

            let Some(mut pkt) = self.output.take() else {
                return Ok(());
            };
            let max_chunk = endpoint.max_writable_payload_len();
            let rem = pkt.bytes().len();
            if rem == 0 {
                let fin = self.output_fin;
                match endpoint.write_stream_packet(pkt, fin, now_us) {
                    Ok(_) => {
                        unsafe {
                            let _ = table.written.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle);
                        }
                        if fin {
                            self.tx_fin = true;
                            if self.rx_fin {
                                self.close_native(hw, Some(table));
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    Err(returned) => {
                        self.output = Some(returned);
                        return Ok(());
                    }
                }
            }

            if max_chunk == 0 {
                self.output = Some(pkt);
                return Ok(());
            }

            if rem <= max_chunk {
                let fin = self.output_fin;
                match endpoint.write_stream_packet(pkt, fin, now_us) {
                    Ok(_) => {
                        unsafe {
                            let _ = table.written.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle);
                        }
                        if fin {
                            self.tx_fin = true;
                            if self.rx_fin {
                                self.close_native(hw, Some(table));
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    Err(returned) => {
                        self.output = Some(returned);
                        return Ok(());
                    }
                }
            } else {
                let prefix_len = if rem <= max_chunk.saturating_mul(2) {
                    rem - max_chunk
                } else {
                    max_chunk
                };
                let r = pkt.range();
                let slice = &pkt.slot_slice()[r.start..r.start + prefix_len];
                match endpoint.write_stream(TxStreamData::Bytes(slice), false, now_us) {
                    Ok(n) if n > 0 => {
                        pkt.set_range(r.start + n..r.end).map_err(|_| ERR_ARG)?;
                        self.output = Some(pkt);
                    }
                    _ => {
                        self.output = Some(pkt);
                        return Ok(());
                    }
                }
            }
        }
    }
}

struct CallbackEnv<'a> {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    session: &'a mut SingleSession,
    errors: &'a mut u64,
    last_error: &'a mut i32,
}

impl SingleStreamCallbacks<XipPacket> for CallbackEnv<'_> {
    fn on_stream_reset(&mut self, _new_stream_id: StreamId) {
        self.session.close_native(self.hw, self.stream_table);
    }

    fn on_stream_data(&mut self, mut chunk: RxStreamChunk<'_, XipPacket>, fin: bool) -> RxOutcome<XipPacket> {
        let Some(table) = self.stream_table else {
            *self.errors += 1;
            *self.last_error = ERR_UNSUPPORTED;
            return RxOutcome::Consumed(0);
        };
        let len = chunk.len();
        let consumed = match self.session.feed(self.hw, table, chunk.bytes(), fin) {
            Ok(n) => n,
            Err(rc) => {
                *self.errors += 1;
                *self.last_error = rc;
                self.session.close_native(self.hw, Some(table));
                return RxOutcome::Consumed(len);
            }
        };
        if consumed < len {
            if let Some(mut pkt) = chunk.take_packet() {
                let r = pkt.range();
                let _ = pkt.set_range((r.start + consumed)..r.end);
                self.session.paused_pkt = Some(pkt);
                self.session.paused_fin = fin;
            }
        }
        RxOutcome::Consumed(consumed)
    }
}

struct Context {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    quic_attached: bool,
    endpoint: SingleStreamEndpoint<XipPool>,
    session: SingleSession,
    active_stream_id: StreamId,
    udp_peer: PeerRoute,
    serial_peer: PeerRoute,
    alarm: i32,
    received_packets: u64,
    sent_packets: u64,
    errors: u64,
    last_error: i32,
}

impl Context {
    #[inline(never)]
    fn new(hw: Hw) -> Self {
        let cfg = default_qlink_config();
        let mut endpoint = SingleStreamEndpoint::new(XipPool::new(hw), cfg);
        endpoint.assoc_mut().upsert_link(LINK_UDP, udp_link_profile());
        endpoint.assoc_mut().upsert_link(LINK_SERIAL, serial_link_profile());
        if let Some(l) = endpoint.assoc_mut().link_mut(LINK_UDP) {
            l.enabled = false;
        }
        if let Some(l) = endpoint.assoc_mut().link_mut(LINK_SERIAL) {
            l.enabled = false;
        }
        let mut cx = Self {
            hw,
            stream_table: hw.stream(),
            quic_attached: false,
            endpoint,
            session: SingleSession::default(),
            active_stream_id: cfg.stream_id,
            udp_peer: PeerRoute::default(),
            serial_peer: PeerRoute::default(),
            alarm: -1,
            received_packets: 0,
            sent_packets: 0,
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
                QLINK_SERIAL_PORT
            } else {
                call.id
            };
            self.serial_peer = PeerRoute {
                active: true,
                link: call.from,
                token,
            };
            if let Some(l) = self.endpoint.assoc_mut().link_mut(LINK_SERIAL) {
                l.enabled = true;
                l.driver_ready = true;
            }
            LINK_SERIAL
        } else {
            self.udp_peer = PeerRoute {
                active: true,
                link: call.from,
                token: call.id,
            };
            if let Some(l) = self.endpoint.assoc_mut().link_mut(LINK_UDP) {
                l.enabled = true;
                l.driver_ready = true;
            }
            LINK_UDP
        };

        self.received_packets += 1;
        let now_us = self.now_us(call.time_us);
        let res = {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                session: &mut self.session,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.endpoint.receive_packet(link_id, pkt, now_us, &mut env)
        };
        if self.endpoint.stream_id() != self.active_stream_id {
            self.active_stream_id = self.endpoint.stream_id();
        }
        res.map(|_| ()).map_err(|_| ERR_IO)
    }

    fn flush_outbound(&mut self, now_us: u64) {
        while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
            let peer = match out.link_id {
                LINK_UDP if self.udp_peer.active => self.udp_peer,
                LINK_SERIAL if self.serial_peer.active => self.serial_peer,
                _ if self.udp_peer.active => self.udp_peer,
                _ if self.serial_peer.active => self.serial_peer,
                _ => {
                    self.endpoint.assoc_mut().on_driver_complete(
                        out.link_id,
                        out.packet_number,
                        quic_link::LinkTxOutcome::Failed,
                        now_us,
                    );
                    continue;
                }
            };
            let outcome = if self.send_native_packet(peer, out.packet).is_ok() {
                quic_link::LinkTxOutcome::Sent
            } else {
                quic_link::LinkTxOutcome::Failed
            };
            self.endpoint.assoc_mut().on_driver_complete(
                out.link_id,
                out.packet_number,
                outcome,
                now_us,
            );
        }
    }

    fn send_native_packet(&mut self, peer: PeerRoute, mut pkt: XipPacket) -> Result<(), i32> {
        let wire_range = pkt.range();
        let wire_len = wire_range.len();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let rc = self.hw.pkt_tx(
                pkt.handle(),
                wire_range.start as u32,
                wire_len as u32,
                peer.link,
                peer.token,
            );
            if rc != xip_abi::ERR_UNSUPPORTED {
                core::mem::forget(pkt);
                if rc == 0 {
                    self.sent_packets += 1;
                    return Ok(());
                }
                return Err(rc);
            }
        }

        let head = PKT_HEADROOM as usize;
        if wire_range.start != head {
            if !pkt.is_unique() {
                if let Ok(mut uniq) = XipPool::new(self.hw).alloc() {
                    if uniq.append(pkt.bytes()).is_ok() {
                        pkt = uniq;
                    } else {
                        return Err(ERR_BUSY);
                    }
                } else {
                    return Err(ERR_BUSY);
                }
            } else if let Ok(slot) = pkt.slot_slice_mut() {
                slot.copy_within(wire_range.clone(), head);
            } else {
                return Err(ERR_BUSY);
            }
        }
        let (handle, base, cap, _) = pkt.into_raw_parts();
        let rc = self
            .hw
            .send(handle, wire_len, peer.link, 0, 0, peer.token);
        if rc == 0 {
            self.sent_packets += 1;
            return Ok(());
        }
        if rc == ERR_BUSY {
            let _restored = unsafe {
                XipPacket::from_raw_parts(self.hw, handle, base, cap, head..head + wire_len)
            };
            return Err(ERR_BUSY);
        }
        Err(rc)
    }

    #[inline(never)]
    fn progress(&mut self, now_us: u64) {
        // 1. Resume paused consumer delivery if needed.
        if self.session.needs_resume {
            if let (Some(table), Some(mut pkt)) =
                (self.stream_table, self.session.paused_pkt.take())
            {
                let fin = self.session.paused_fin;
                let r = pkt.range();
                let len = r.len();
                match self
                    .session
                    .feed(self.hw, table, &pkt.slot_slice()[r.clone()], fin)
                {
                    Ok(consumed) => {
                        if consumed < len {
                            let _ = pkt.set_range((r.start + consumed)..r.end);
                            self.session.paused_pkt = Some(pkt);
                            self.session.paused_fin = fin;
                            self.endpoint.credit_consumed_bytes(consumed, now_us);
                        } else {
                            self.session.paused_fin = false;
                            let mut env = CallbackEnv {
                                hw: self.hw,
                                stream_table: self.stream_table,
                                session: &mut self.session,
                                errors: &mut self.errors,
                                last_error: &mut self.last_error,
                            };
                            self.endpoint.resume_stream(consumed, now_us, &mut env);
                        }
                    }
                    Err(rc) => {
                        self.errors += 1;
                        self.last_error = rc;
                        self.session.close_native(self.hw, Some(table));
                    }
                }
            } else {
                self.session.needs_resume = false;
            }
        }

        // 2. Pump consumer stream egress (`c_stream_take` -> `SingleStreamEndpoint`).
        if let Some(table) = self.stream_table {
            if let Err(rc) = self
                .session
                .pump_egress(self.hw, table, &mut self.endpoint, now_us)
            {
                self.errors += 1;
                self.last_error = rc;
            }
        }

        // 3. Poll timers (`ACK`, SACK loss, PTO).
        let _ = self.endpoint.poll_timers(now_us);

        // 4. Transmit queued packets onto UDP / PPP serial.
        self.flush_outbound(now_us);
    }

    fn update_alarm(&mut self, now_us: u64) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        let has_active_session = self.session.is_active();
        let next_dl = self.endpoint.assoc().next_deadline_us();
        if !self.quic_attached {
            self.alarm = self
                .hw
                .alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            return;
        }
        if !has_active_session && next_dl.is_none() {
            return;
        }
        let delay_ms = match (has_active_session, next_dl) {
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
        self.session.close_native(self.hw, self.stream_table);
        self.detach_quic();
    }

    fn reset(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        self.session.close_native(self.hw, self.stream_table);
        self.udp_peer = PeerRoute::default();
        self.serial_peer = PeerRoute::default();
        let cfg = default_qlink_config();
        self.endpoint = SingleStreamEndpoint::new(XipPool::new(self.hw), cfg);
        self.endpoint
            .assoc_mut()
            .upsert_link(LINK_UDP, udp_link_profile());
        self.endpoint
            .assoc_mut()
            .upsert_link(LINK_SERIAL, serial_link_profile());
        if let Some(l) = self.endpoint.assoc_mut().link_mut(LINK_UDP) {
            l.enabled = false;
        }
        if let Some(l) = self.endpoint.assoc_mut().link_mut(LINK_SERIAL) {
            l.enabled = false;
        }
        self.active_stream_id = cfg.stream_id;
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

        if call.flags & CALL_LINK != 0 || !call.wants_response() {
            return RET_CONTINUE as i32;
        }

        let mut out = [0u8; 160];
        let mut w = Writer::new(&mut out);
        match call.method {
            1 => {
                cx.ensure_quic_attached();
                let retained = cx.endpoint.assoc().sack_ledger().retained_packets() as u64;
                w.map(6)
                    .uint(1)
                    .boolean(cx.session.is_active())
                    .uint(2)
                    .uint(cx.received_packets)
                    .uint(3)
                    .uint(cx.sent_packets)
                    .uint(4)
                    .uint(retained)
                    .uint(5)
                    .uint(cx.errors)
                    .uint(6)
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
