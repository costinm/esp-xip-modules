//! TCP (`--stream 4`) and PPP serial (`--port 245`) link framing and egress for `QuicMux`.

use alloc::vec::Vec;
use quic_mux::{
    default_coalesce_threshold, try_coalesce_tx_packet,
    wire::{plan_stream_packet, StreamPacketPlan, MAX_STREAM_FRAME_HEADER_LEN},
    AssociationEgress, AssociationId, LinkSendError, PacketPool, PoolPacket, TxPacketMeta,
};
pub use quic_mux::OWNER_ASSOC;
use xip_abi::{Hw, ERR_AGAIN, ERR_ARG, ERR_BUSY, PKT_HEADROOM, PKT_MAX};

use crate::pool::{XipPacket, XipPool};

/// PPP port `245` (`0xf5`) used for `mod_mux` serial frames.
pub const MUX_SERIAL_PORT: u64 = 245;
/// Maximum `quic-mux` packet bytes over PPP serial (`1500 == PKT_MAX`).
pub const SERIAL_FRAME_MTU: usize = PKT_MAX as usize;
/// Maximum `quic-mux` packet bytes over TCP (`1500 + 32 == 1532`, matching `TokioMux`).
pub const TCP_FRAME_MTU: usize = (PKT_MAX as usize) + 32;
/// Maximum concurrent TCP bearer sockets retained by `mod_mux`.
pub const MAX_TCP_SOCKETS: usize = 4;
/// Pool slots reserved for RX when allocating TCP assembly packets.
const RX_POOL_RESERVE: usize = 2;

pub(crate) struct TcpLink {
    pub socket: u32,
    pub assoc: AssociationId,
    peek: [u8; MAX_STREAM_FRAME_HEADER_LEN],
    peek_len: usize,
    rx_pkt: Option<XipPacket>,
    rx_target: usize,
    rx_at: usize,
    tx_inflight: bool,
    pending_tx: Option<(TxPacketMeta, XipPacket)>,
    pub peer_eof: bool,
}

impl TcpLink {
    pub fn new(socket: u32) -> Self {
        Self {
            socket,
            assoc: OWNER_ASSOC,
            peek: [0; MAX_STREAM_FRAME_HEADER_LEN],
            peek_len: 0,
            rx_pkt: None,
            rx_target: 0,
            rx_at: 0,
            tx_inflight: false,
            pending_tx: None,
            peer_eof: false,
        }
    }

    #[inline]
    pub fn can_accept_tx(&self) -> bool {
        !self.tx_inflight || self.pending_tx.is_none()
    }

    pub fn close(mut self, hw: Hw) {
        self.rx_pkt = None;
        self.pending_tx = None;
        hw.socket_close(self.socket);
    }
}

#[derive(Clone, Copy)]
struct SerialPeer {
    assoc: AssociationId,
    link: u32,
    port: u64,
}

pub(crate) struct MuxLinks {
    pub hw: Hw,
    pub tcp: Vec<TcpLink>,
    tcp_rr: usize,
    serial_peers: Vec<SerialPeer>,
    pub sent_frames: u64,
    pub coalesced_packets: u64,
}

impl MuxLinks {
    pub fn new(hw: Hw) -> Self {
        Self {
            hw,
            tcp: Vec::new(),
            tcp_rr: 0,
            serial_peers: Vec::new(),
            sent_frames: 0,
            coalesced_packets: 0,
        }
    }

    pub fn close_all(&mut self) {
        let hw = self.hw;
        for link in core::mem::take(&mut self.tcp) {
            link.close(hw);
        }
        self.serial_peers.clear();
    }

    pub fn tcp_count(&self) -> u64 {
        self.tcp.len() as u64
    }

    pub fn note_serial_peer(&mut self, assoc: AssociationId, link: u32, token: u64) {
        let port = if token == 0 { MUX_SERIAL_PORT } else { token };
        if let Some(p) = self.serial_peers.iter_mut().find(|p| p.assoc == assoc) {
            p.link = link;
            p.port = port;
            return;
        }
        if self.serial_peers.len() >= 8 {
            self.serial_peers.remove(0);
        }
        self.serial_peers.push(SerialPeer { assoc, link, port });
    }

    pub fn add_tcp_socket(&mut self, socket: u32) {
        if self.tcp.iter().any(|t| t.socket == socket) {
            return;
        }
        if self.tcp.len() >= MAX_TCP_SOCKETS {
            self.hw.socket_close(socket);
            return;
        }
        self.tcp.push(TcpLink::new(socket));
    }

    pub fn remove_tcp_socket(&mut self, socket: u32) -> Option<AssociationId> {
        let pos = self.tcp.iter().position(|t| t.socket == socket)?;
        let link = self.tcp.swap_remove(pos);
        let assoc = link.assoc;
        link.close(self.hw);
        Some(assoc)
    }

    pub fn has_any_bearer(&self, assoc: AssociationId) -> bool {
        self.tcp.iter().any(|t| t.assoc == assoc)
            || self.serial_peers.iter().any(|p| p.assoc == assoc)
    }

    pub fn on_tcp_write_done(&mut self, socket: u32) -> Option<AssociationId> {
        let idx = self.tcp.iter().position(|t| t.socket == socket)?;
        self.tcp[idx].tx_inflight = false;
        let assoc = self.tcp[idx].assoc;
        if let Err(_rc) = self.flush_tcp_idx(idx) {
            return self.remove_tcp_socket(socket).or(Some(assoc));
        }
        Some(assoc)
    }

    pub fn on_tcp_writable(&mut self, socket: u32) -> Option<AssociationId> {
        let idx = self.tcp.iter().position(|t| t.socket == socket)?;
        let assoc = self.tcp[idx].assoc;
        if !self.tcp[idx].tx_inflight && self.tcp[idx].pending_tx.is_some() {
            if self.flush_tcp_idx(idx).is_err() {
                return self.remove_tcp_socket(socket).or(Some(assoc));
            }
        }
        Some(assoc)
    }

    fn flush_tcp_idx(&mut self, idx: usize) -> Result<(), i32> {
        if self.tcp[idx].tx_inflight {
            return Ok(());
        }
        let Some((meta, pkt)) = self.tcp[idx].pending_tx.take() else {
            return Ok(());
        };
        match Self::submit_tcp_pkt(self.hw, self.tcp[idx].socket, &mut self.tcp[idx].tx_inflight, pkt) {
            Ok(()) => {
                self.sent_frames += 1;
                Ok(())
            }
            Err((ERR_BUSY, returned)) => {
                self.tcp[idx].pending_tx = Some((meta, returned));
                Ok(())
            }
            Err((rc, _dropped)) => Err(rc),
        }
    }

    fn submit_tcp_pkt(
        hw: Hw,
        socket: u32,
        tx_inflight: &mut bool,
        pkt: XipPacket,
    ) -> Result<(), (i32, XipPacket)> {
        let wire_range = pkt.range();
        let (handle, base, cap, _) = pkt.into_raw_parts();
        let rc = hw.socket_write_pkt(
            socket,
            handle,
            wire_range.start as u32,
            wire_range.len() as u32,
        );
        if rc == 0 {
            *tx_inflight = true;
            Ok(())
        } else {
            let restored = unsafe { XipPacket::from_raw_parts(hw, handle, base, cap, wire_range) };
            Err((rc, restored))
        }
    }

    /// Read available self-delimiting QUIC frames from `socket`, invoking `on_frame` for each packet.
    /// Returns `Ok(closed)` where `closed == true` means the TCP socket reached clean EOF and should be closed.
    pub fn drain_tcp_reads<F>(
        &mut self,
        pool: &XipPool,
        socket: u32,
        mut on_frame: F,
    ) -> Result<bool, i32>
    where
        F: FnMut(&mut Self, AssociationId, XipPacket) -> Result<(), i32>,
    {
        loop {
            let Some(idx) = self.tcp.iter().position(|t| t.socket == socket) else {
                return Ok(true);
            };
            while self.tcp[idx].rx_pkt.is_none() {
                let peek_len = self.tcp[idx].peek_len;
                match plan_stream_packet(&self.tcp[idx].peek[..peek_len], TCP_FRAME_MTU) {
                    Ok(StreamPacketPlan::Ready { total_len }) => {
                        let pkt = match pool.alloc_reserving(RX_POOL_RESERVE) {
                            Ok(p) => p,
                            Err(_) => match pool.alloc() {
                                Ok(p) => p,
                                Err(_) => return Ok(false),
                            },
                        };
                        self.tcp[idx].rx_pkt = Some(pkt);
                        self.tcp[idx].rx_target = total_len;
                        self.tcp[idx].rx_at = 0;
                        break;
                    }
                    Ok(StreamPacketPlan::NeedMoreHeader) => {
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
                            return Ok(
                                !self.tcp[idx].tx_inflight && self.tcp[idx].pending_tx.is_none()
                            );
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

            let assoc = self.tcp[idx].assoc;
            self.tcp[idx].rx_target = 0;
            self.tcp[idx].rx_at = 0;
            let mut pkt = self.tcp[idx].rx_pkt.take().unwrap();
            pkt.set_range(rx_start..rx_start + target)
                .map_err(|_| ERR_ARG)?;
            on_frame(self, assoc, pkt)?;
        }
    }

    fn send_via_serial(
        &mut self,
        peer: SerialPeer,
        mut pkt: XipPacket,
    ) -> Result<(), LinkSendError<XipPacket>> {
        let orig_range = pkt.range();
        if orig_range.len() > SERIAL_FRAME_MTU {
            return Err(LinkSendError::TooLarge {
                packet: pkt,
                max_frame_len: SERIAL_FRAME_MTU,
            });
        }
        let wire_range = orig_range.clone();
        let wire_len = wire_range.len();

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pkt_tx) = unsafe { (*self.hw.as_ptr()).pkt_tx } {
            let (handle, base, cap, _) = pkt.into_raw_parts();
            let rc = unsafe {
                pkt_tx(
                    self.hw.ctx(),
                    handle,
                    wire_range.start as u32,
                    wire_len as u32,
                    peer.link,
                    peer.port,
                )
            };
            if rc == 0 {
                self.sent_frames += 1;
                return Ok(());
            }
            if rc == ERR_BUSY {
                let mut restored =
                    unsafe { XipPacket::from_raw_parts(self.hw, handle, base, cap, wire_range) };
                let _ = restored.set_range(orig_range);
                return Err(LinkSendError::Busy(restored));
            }
            // Non-BUSY error already consumed `handle` inside `pkt_tx`.
            return Ok(());
        }

        // Fallback (including wasm32 host runtime): `Hw::send` reads `len` bytes from `PKT_HEADROOM`.
        let head = PKT_HEADROOM as usize;
        if wire_range.start != head {
            if let Ok(slot) = pkt.slot_slice_mut() {
                slot.copy_within(wire_range.clone(), head);
            } else {
                let _ = pkt.set_range(orig_range);
                return Err(LinkSendError::Busy(pkt));
            }
        }
        let (handle, base, cap, _) = pkt.into_raw_parts();
        let rc = self.hw.send(handle, wire_len, peer.link, 0, 0, peer.port);
        if rc == 0 {
            self.sent_frames += 1;
            return Ok(());
        }
        if rc == ERR_BUSY {
            let mut restored = unsafe {
                XipPacket::from_raw_parts(self.hw, handle, base, cap, head..head + wire_len)
            };
            if wire_range.start != head {
                if let Ok(slot) = restored.slot_slice_mut() {
                    slot.copy_within(head..head + wire_len, wire_range.start);
                }
            }
            let _ = restored.set_range(orig_range);
            return Err(LinkSendError::Busy(restored));
        }
        Ok(())
    }
}

impl AssociationEgress<XipPacket> for MuxLinks {
    fn max_writable_frame_len(&self, assoc: AssociationId) -> usize {
        let has_tcp = self.tcp.iter().any(|t| {
            t.assoc == assoc
                && (t.can_accept_tx()
                    || t.pending_tx
                        .as_ref()
                        .is_some_and(|(_, p)| p.bytes().len() < TCP_FRAME_MTU))
        });
        let has_serial = self.serial_peers.iter().any(|p| p.assoc == assoc);
        if has_tcp && has_serial {
            // Multipath association: cap to SERIAL_FRAME_MTU so failover fits either bearer.
            SERIAL_FRAME_MTU
        } else if has_tcp {
            TCP_FRAME_MTU
        } else if has_serial {
            SERIAL_FRAME_MTU
        } else {
            0
        }
    }

    fn send_packet(
        &mut self,
        meta: TxPacketMeta,
        mut packet: XipPacket,
    ) -> Result<(), LinkSendError<XipPacket>> {
        // 1. Prefer connected TCP bearer(s) for `meta.association` (cost 1, round-robin across open TCP streams).
        let assoc = meta.association;
        let has_serial = self.serial_peers.iter().any(|p| p.assoc == assoc);
        let tcp_mtu = if has_serial {
            SERIAL_FRAME_MTU
        } else {
            TCP_FRAME_MTU
        };

        if packet.bytes().len() <= tcp_mtu && !self.tcp.is_empty() {
            // Pass 1: try any immediately idle TCP stream in round-robin order.
            let n = self.tcp.len();
            let start = self.tcp_rr % n;
            let mut i = 0;
            while i < n && !self.tcp.is_empty() {
                let idx = (start + i) % self.tcp.len();
                if self.tcp[idx].assoc == assoc
                    && !self.tcp[idx].tx_inflight
                    && self.tcp[idx].pending_tx.is_none()
                {
                    match Self::submit_tcp_pkt(
                        self.hw,
                        self.tcp[idx].socket,
                        &mut self.tcp[idx].tx_inflight,
                        packet,
                    ) {
                        Ok(()) => {
                            self.tcp_rr = idx + 1;
                            self.sent_frames += 1;
                            return Ok(());
                        }
                        Err((ERR_BUSY, returned)) => {
                            packet = returned;
                            i += 1;
                        }
                        Err((_rc, returned)) => {
                            let dead_sock = self.tcp[idx].socket;
                            self.remove_tcp_socket(dead_sock);
                            packet = returned;
                        }
                    }
                } else {
                    i += 1;
                }
            }

            // Pass 2: coalesce into or queue behind an in-flight TCP stream.
            if !self.tcp.is_empty() {
                let n = self.tcp.len();
                let start = self.tcp_rr % n;
                for offset in 0..n {
                    let idx = (start + offset) % n;
                    if self.tcp[idx].assoc != assoc {
                        continue;
                    }
                    if let Some((tail_meta, tail_pkt)) = self.tcp[idx].pending_tx.as_mut() {
                        match try_coalesce_tx_packet(
                            tail_meta,
                            tail_pkt,
                            meta,
                            packet,
                            tcp_mtu,
                            default_coalesce_threshold(tcp_mtu),
                        ) {
                            Ok(()) => {
                                self.coalesced_packets += 1;
                                return Ok(());
                            }
                            Err(returned) => {
                                packet = returned;
                            }
                        }
                    } else if self.tcp[idx].tx_inflight {
                        self.tcp_rr = idx + 1;
                        self.tcp[idx].pending_tx = Some((meta, packet));
                        return Ok(());
                    }
                }
            }
        }

        // 2. Failover / primary PPP serial bearer (`--port 245`, cost 2).
        if let Some(&peer) = self.serial_peers.iter().find(|p| p.assoc == assoc) {
            return self.send_via_serial(peer, packet);
        }

        if self.tcp.iter().any(|t| t.assoc == assoc) {
            Err(LinkSendError::Busy(packet))
        } else {
            Err(LinkSendError::NoAssociation(packet))
        }
    }
}
