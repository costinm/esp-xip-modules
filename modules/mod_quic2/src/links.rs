//! UDP, PPP serial (`--port 243`), and TCP (`--stream 6`) physical link drivers for `QuicLinkAssoc`.

use alloc::vec::Vec;
use quic_link::{
    decode_tls_record_header, encode_tls_record_header, CongestionMode, ConnectionId, LinkFraming,
    LinkId, LinkProfile, LinkTxOutcome, QuicLinkAssoc, TLS_RECORD_HEADER_LEN,
};
use quic_mux::{PacketPool, PoolPacket, StreamPriority};
pub use quic_mux::OWNER_ASSOC;
use xip_abi::{Hw, ERR_AGAIN, ERR_ARG, ERR_BUSY, PKT_HEADROOM, PKT_MAX};

use crate::pool::{XipPacket, XipPool};

/// PPP port `243` (`0xf3`) used for `mod_quic2` serial frames.
pub const QUIC2_SERIAL_PORT: u64 = 243;
/// Link ID for UDP datagram bearer.
pub const LINK_UDP: LinkId = LinkId(1);
/// Link ID for PPP serial bearer.
pub const LINK_SERIAL: LinkId = LinkId(2);
/// Base Link ID (`LinkId(10 + idx)`) for TCP stream sockets on `--stream 6`.
pub const LINK_TCP_BASE: u16 = 10;

/// Maximum datagram/stream QUIC packet bytes across UDP, PPP serial, TCP, and NOW (`1472`, fitting `PKT_MAX = 1500` and unfragmented IPv4 UDP).
pub const UNIFIED_LINK_MTU: usize = 1472;
/// Maximum packet bytes over UDP (`1472`, unfragmented IPv4 UDP payload within Ethernet 1500 MTU).
pub const UDP_FRAME_MTU: usize = UNIFIED_LINK_MTU;
/// Maximum packet bytes over PPP serial (matches [`UDP_FRAME_MTU`] so cross-link failover fits UDP/NOW/TCP).
pub const SERIAL_FRAME_MTU: usize = UNIFIED_LINK_MTU;
/// Maximum packet bytes over TCP (matches [`UDP_FRAME_MTU`] so cross-link failover fits serial/UDP/NOW).
pub const TCP_FRAME_MTU: usize = UNIFIED_LINK_MTU;
/// Maximum concurrent TCP bearer sockets retained by `mod_quic2`.
pub const MAX_TCP_SOCKETS: usize = 4;
const PEEK_CAP: usize = 64;
const RX_POOL_RESERVE: usize = 2;

pub fn udp_link_profile() -> LinkProfile {
    udp_link_profile_with_encrypt(true)
}

pub fn udp_link_profile_with_encrypt(encrypt: bool) -> LinkProfile {
    let cid = ConnectionId::new(1).unwrap();
    LinkProfile {
        tier: 0,
        mtu: UDP_FRAME_MTU,
        max_priority: StreamPriority::BULK,
        congestion_mode: CongestionMode::FlowControlled {
            max_inflight_packets: 4,
            max_inflight_bytes: 6 * 1024,
        },
        framing: LinkFraming::Datagram { peer_cid: cid },
        initial_rtt_us: 25_000,
        encrypt,
    }
}

pub fn tcp_link_profile() -> LinkProfile {
    tcp_link_profile_with_encrypt(false)
}

pub fn tcp_link_profile_with_encrypt(encrypt: bool) -> LinkProfile {
    let cid = ConnectionId::new(1).unwrap();
    LinkProfile {
        tier: 0,
        mtu: TCP_FRAME_MTU,
        max_priority: StreamPriority::BULK,
        congestion_mode: CongestionMode::FlowControlled {
            max_inflight_packets: 6,
            max_inflight_bytes: 9 * 1024,
        },
        framing: LinkFraming::TlsRecord { peer_cid: cid },
        initial_rtt_us: 20_000,
        encrypt,
    }
}

pub fn serial_link_profile() -> LinkProfile {
    serial_link_profile_with_encrypt(false)
}

pub fn serial_link_profile_with_encrypt(encrypt: bool) -> LinkProfile {
    let cid = ConnectionId::new(1).unwrap();
    LinkProfile {
        tier: 1,
        mtu: SERIAL_FRAME_MTU,
        max_priority: StreamPriority::BULK,
        congestion_mode: CongestionMode::FlowControlled {
            max_inflight_packets: 4,
            max_inflight_bytes: 5 * 1024,
        },
        framing: LinkFraming::Datagram { peer_cid: cid },
        initial_rtt_us: 150_000,
        encrypt,
    }
}

#[derive(Clone, Copy, Default)]
pub struct PeerRoute {
    pub active: bool,
    pub link: u32,
    pub token: u64,
}

pub(crate) struct TcpSocketBearer {
    pub socket: u32,
    pub link_id: LinkId,
    peek: [u8; PEEK_CAP],
    peek_len: usize,
    rx_pkt: Option<XipPacket>,
    rx_target: usize,
    rx_at: usize,
    pub tx_inflight_pkt_num: Option<u32>,
    pub pending_tx: Vec<(u32, XipPacket)>,
    pub peer_eof: bool,
}

impl TcpSocketBearer {
    pub fn new(socket: u32, link_id: LinkId) -> Self {
        Self {
            socket,
            link_id,
            peek: [0; PEEK_CAP],
            peek_len: 0,
            rx_pkt: None,
            rx_target: 0,
            rx_at: 0,
            tx_inflight_pkt_num: None,
            pending_tx: Vec::new(),
            peer_eof: false,
        }
    }

    #[inline]
    pub fn is_tx_idle(&self) -> bool {
        self.tx_inflight_pkt_num.is_none() && self.pending_tx.is_empty()
    }

    pub fn close(mut self, hw: Hw) {
        self.rx_pkt = None;
        self.pending_tx.clear();
        hw.socket_close(self.socket);
    }
}

pub(crate) struct Quic2Drivers {
    pub hw: Hw,
    pub udp_peer: PeerRoute,
    pub serial_peer: PeerRoute,
    pub tcp: Vec<TcpSocketBearer>,
    next_tcp_slot: u16,
    pub sent_packets: u64,
}

impl Quic2Drivers {
    pub fn new(hw: Hw) -> Self {
        Self {
            hw,
            udp_peer: PeerRoute::default(),
            serial_peer: PeerRoute::default(),
            tcp: Vec::new(),
            next_tcp_slot: 0,
            sent_packets: 0,
        }
    }

    pub fn close_all(&mut self) {
        let hw = self.hw;
        for t in core::mem::take(&mut self.tcp) {
            t.close(hw);
        }
        self.udp_peer = PeerRoute::default();
        self.serial_peer = PeerRoute::default();
    }

    pub fn tcp_count(&self) -> u64 {
        self.tcp.len() as u64
    }

    pub fn add_tcp_socket(
        &mut self,
        socket: u32,
        encrypt: bool,
        assoc: &mut QuicLinkAssoc<XipPacket>,
    ) -> Option<LinkId> {
        if let Some(existing) = self.tcp.iter().find(|t| t.socket == socket) {
            return Some(existing.link_id);
        }
        if self.tcp.len() >= MAX_TCP_SOCKETS {
            self.hw.socket_close(socket);
            return None;
        }
        let link_id = LinkId(LINK_TCP_BASE + (self.next_tcp_slot % 16));
        self.next_tcp_slot = self.next_tcp_slot.wrapping_add(1);
        assoc.upsert_link(link_id, tcp_link_profile_with_encrypt(encrypt));
        self.tcp.push(TcpSocketBearer::new(socket, link_id));
        Some(link_id)
    }

    pub fn remove_tcp_socket(
        &mut self,
        socket: u32,
        assoc: &mut QuicLinkAssoc<XipPacket>,
        now_us: u64,
    ) {
        let Some(pos) = self.tcp.iter().position(|t| t.socket == socket) else {
            return;
        };
        let bearer = self.tcp.swap_remove(pos);
        let link_id = bearer.link_id;
        bearer.close(self.hw);
        assoc.fail_link(link_id, true, now_us);
    }

    pub fn on_tcp_write_done(
        &mut self,
        socket: u32,
        assoc: &mut QuicLinkAssoc<XipPacket>,
        now_us: u64,
    ) {
        let Some(idx) = self.tcp.iter().position(|t| t.socket == socket) else {
            return;
        };
        let link_id = self.tcp[idx].link_id;
        if let Some(pn) = self.tcp[idx].tx_inflight_pkt_num.take() {
            assoc.on_driver_complete(link_id, pn, LinkTxOutcome::Sent, now_us);
        }
        if self.flush_tcp_pending(idx, assoc, now_us).is_err() {
            self.remove_tcp_socket(socket, assoc, now_us);
        }
    }

    pub fn on_tcp_writable(
        &mut self,
        socket: u32,
        assoc: &mut QuicLinkAssoc<XipPacket>,
        now_us: u64,
    ) {
        let Some(idx) = self.tcp.iter().position(|t| t.socket == socket) else {
            return;
        };
        let link_id = self.tcp[idx].link_id;
        if let Some(l) = assoc.link_mut(link_id) {
            l.driver_ready = true;
        }
        if self.flush_tcp_pending(idx, assoc, now_us).is_err() {
            self.remove_tcp_socket(socket, assoc, now_us);
        }
    }

    fn flush_tcp_pending(
        &mut self,
        idx: usize,
        assoc: &mut QuicLinkAssoc<XipPacket>,
        now_us: u64,
    ) -> Result<(), i32> {
        if self.tcp[idx].tx_inflight_pkt_num.is_some() || self.tcp[idx].pending_tx.is_empty() {
            return Ok(());
        }
        let (pn, pkt) = self.tcp[idx].pending_tx.remove(0);
        let socket = self.tcp[idx].socket;
        let link_id = self.tcp[idx].link_id;
        match Self::submit_tcp_pkt(self.hw, socket, pkt) {
            Ok(()) => {
                self.tcp[idx].tx_inflight_pkt_num = Some(pn);
                self.sent_packets += 1;
                Ok(())
            }
            Err((ERR_BUSY, returned)) => {
                self.tcp[idx].pending_tx.insert(0, (pn, returned));
                Ok(())
            }
            Err((rc, _dropped)) => {
                assoc.on_driver_complete(link_id, pn, LinkTxOutcome::Failed, now_us);
                Err(rc)
            }
        }
    }

    fn submit_tcp_pkt(
        hw: Hw,
        socket: u32,
        mut pkt: XipPacket,
    ) -> Result<(), (i32, XipPacket)> {
        if !pkt.is_unique() {
            if let Ok(mut uniq) = XipPool::new(hw).alloc() {
                if uniq.append(pkt.bytes()).is_ok() {
                    pkt = uniq;
                }
            }
        }
        let orig_range = pkt.range();
        let Some(tls_hdr) = encode_tls_record_header(orig_range.len()) else {
            return Err((ERR_ARG, pkt));
        };
        if pkt.prepend(&tls_hdr).is_err() {
            return Err((ERR_ARG, pkt));
        }
        let wire_range = pkt.range();
        let (handle, base, cap, _) = pkt.into_raw_parts();
        let rc = hw.socket_write_pkt(
            socket,
            handle,
            wire_range.start as u32,
            wire_range.len() as u32,
        );
        if rc == 0 {
            Ok(())
        } else {
            let restored = unsafe { XipPacket::from_raw_parts(hw, handle, base, cap, orig_range) };
            Err((rc, restored))
        }
    }

    /// Read self-delimiting `ShortHeader` + QUIC frames from `socket`, invoking `on_packet` for each packet.
    pub fn drain_tcp_reads<F>(
        &mut self,
        pool: &XipPool,
        socket: u32,
        mut on_packet: F,
    ) -> Result<bool, i32>
    where
        F: FnMut(LinkId, XipPacket) -> Result<(), i32>,
    {
        loop {
            let Some(idx) = self.tcp.iter().position(|t| t.socket == socket) else {
                return Ok(true);
            };
            while self.tcp[idx].rx_pkt.is_none() {
                let peek_len = self.tcp[idx].peek_len;
                match decode_tls_record_header(&self.tcp[idx].peek[..peek_len], PKT_MAX as usize) {
                    Ok(Some(total_len)) => {
                        let pkt = match pool.alloc_reserving(RX_POOL_RESERVE) {
                            Ok(p) => p,
                            Err(_) => match pool.alloc() {
                                Ok(p) => p,
                                Err(_) => return Ok(false),
                            },
                        };
                        let rem = peek_len - TLS_RECORD_HEADER_LEN;
                        self.tcp[idx]
                            .peek
                            .copy_within(TLS_RECORD_HEADER_LEN..peek_len, 0);
                        self.tcp[idx].peek_len = rem;
                        self.tcp[idx].rx_pkt = Some(pkt);
                        self.tcp[idx].rx_target = total_len;
                        self.tcp[idx].rx_at = 0;
                        break;
                    }
                    Ok(None) => {
                        let n = self
                            .hw
                            .socket_read(socket, &mut self.tcp[idx].peek[peek_len..]);
                        if n == ERR_AGAIN {
                            return Ok(false);
                        }
                        if n < 0 {
                            return Err(n);
                        }
                        if n == 0 {
                            if peek_len != 0 {
                                return Err(ERR_ARG);
                            }
                            self.tcp[idx].peer_eof = true;
                            return Ok(self.tcp[idx].is_tx_idle());
                        }
                        self.tcp[idx].peek_len += n as usize;
                    }
                    Err(_) => return Err(ERR_ARG),
                }
            }

            let headroom = pool.headroom();
            let slot_cap = headroom + pool.max_payload();
            let target = self.tcp[idx].rx_target;
            if target > slot_cap {
                return Err(ERR_ARG);
            }
            let rx_start = (slot_cap - target).min(headroom.saturating_sub(8));
            if self.tcp[idx].rx_at == 0 && self.tcp[idx].peek_len > 0 {
                let take = target.min(self.tcp[idx].peek_len);
                {
                    let link = &mut self.tcp[idx];
                    let pkt = link.rx_pkt.as_mut().unwrap();
                    let slot = pkt.slot_slice_mut().map_err(|_| ERR_BUSY)?;
                    slot[rx_start..rx_start + take].copy_from_slice(&link.peek[..take]);
                }
                let old_peek_len = self.tcp[idx].peek_len;
                if take < old_peek_len {
                    self.tcp[idx].peek.copy_within(take..old_peek_len, 0);
                }
                self.tcp[idx].peek_len = old_peek_len - take;
                self.tcp[idx].rx_at = take;
            }

            if self.tcp[idx].rx_at < target {
                let at = self.tcp[idx].rx_at;
                let rem = target - at;
                let read_rc = {
                    let pkt = self.tcp[idx].rx_pkt.as_mut().unwrap();
                    let slot = pkt.slot_slice_mut().map_err(|_| ERR_BUSY)?;
                    self.hw
                        .socket_read(socket, &mut slot[rx_start + at..rx_start + target])
                };
                if read_rc == ERR_AGAIN {
                    return Ok(false);
                }
                if read_rc <= 0 {
                    return Err(if read_rc == 0 { ERR_ARG } else { read_rc });
                }
                let got = read_rc as usize;
                if got > rem {
                    return Err(ERR_ARG);
                }
                self.tcp[idx].rx_at += got;
                if self.tcp[idx].rx_at < target {
                    continue;
                }
            }

            let link_id = self.tcp[idx].link_id;
            self.tcp[idx].rx_target = 0;
            self.tcp[idx].rx_at = 0;
            let mut pkt = self.tcp[idx].rx_pkt.take().unwrap();
            pkt.set_range(rx_start..rx_start + target)
                .map_err(|_| ERR_ARG)?;
            on_packet(link_id, pkt)?;
        }
    }

    pub fn flush_outbound_for_slot(
        &mut self,
        assoc: &mut QuicLinkAssoc<XipPacket>,
        udp_peer: PeerRoute,
        serial_peer: PeerRoute,
        allow_tcp: bool,
        now_us: u64,
    ) {
        while let Some(out) = assoc.pop_outbound_packet() {
            if allow_tcp {
                if let Some(idx) = self.tcp.iter().position(|t| t.link_id == out.link_id) {
                    if self.tcp[idx].tx_inflight_pkt_num.is_none()
                        && self.tcp[idx].pending_tx.is_empty()
                    {
                        let socket = self.tcp[idx].socket;
                        match Self::submit_tcp_pkt(self.hw, socket, out.packet) {
                            Ok(()) => {
                                self.tcp[idx].tx_inflight_pkt_num = Some(out.packet_number);
                                self.sent_packets += 1;
                            }
                            Err((ERR_BUSY, returned)) => {
                                self.tcp[idx]
                                    .pending_tx
                                    .push((out.packet_number, returned));
                            }
                            Err((_rc, _dropped)) => {
                                let dead = self.tcp[idx].socket;
                                assoc.on_driver_complete(
                                    out.link_id,
                                    out.packet_number,
                                    LinkTxOutcome::Failed,
                                    now_us,
                                );
                                self.remove_tcp_socket(dead, assoc, now_us);
                            }
                        }
                    } else if self.tcp[idx].pending_tx.len() < 16 {
                        self.tcp[idx]
                            .pending_tx
                            .push((out.packet_number, out.packet));
                    } else {
                        assoc.on_driver_complete(
                            out.link_id,
                            out.packet_number,
                            LinkTxOutcome::Failed,
                            now_us,
                        );
                    }
                    continue;
                }
            }

            let peer = match out.link_id {
                LINK_UDP if udp_peer.active => udp_peer,
                LINK_SERIAL if serial_peer.active => serial_peer,
                _ if udp_peer.active => udp_peer,
                _ if serial_peer.active => serial_peer,
                _ => {
                    assoc.on_driver_complete(
                        out.link_id,
                        out.packet_number,
                        LinkTxOutcome::Failed,
                        now_us,
                    );
                    continue;
                }
            };
            match self.send_datagram_link(peer, out.packet) {
                Ok(()) => {
                    assoc.on_driver_complete(
                        out.link_id,
                        out.packet_number,
                        LinkTxOutcome::Sent,
                        now_us,
                    );
                }
                Err(_) => {
                    assoc.on_driver_complete(
                        out.link_id,
                        out.packet_number,
                        LinkTxOutcome::Failed,
                        now_us,
                    );
                }
            }
        }
    }

    fn send_datagram_link(&mut self, peer: PeerRoute, mut pkt: XipPacket) -> Result<(), i32> {
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
}
