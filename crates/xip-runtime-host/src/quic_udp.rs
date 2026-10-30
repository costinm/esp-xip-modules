//! The link `udp` of `xip-host`: one UDP socket whose datagrams are frames of a native link, so the classifier (`quic.rs`) sees what the C
//! runtime's `xip_quic_udp.c` hands it. The peer address of a frame is its token (slot and generation in a small table); replies come back through
//! [`Runtime::take_link_tx`]. The port is chosen by the caller (other processes may use 3339).
use crate::{NativeTx, Runtime};
use std::io;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

/// The default QUIC datagram port of the host runtime. Not 3339: lmesh, Android and the firmware serve that, and `dmesh-cli` has its own
/// client port (3338), so a host runtime next to them takes the next one.
pub const DEFAULT_PORT: u16 = 3340;
const PEERS: usize = 64;
const MTU: u32 = 1448;

pub struct QuicUdp {
    sock: UdpSocket,
    peers: Vec<Peer>,
    clock: u64,
}

struct Peer {
    addr: Option<SocketAddr>,
    gen: u32,
    used: u64,
}

impl QuicUdp {
    /// Bind `addr` (port 0: any), create the link `udp` and switch the classifier on.
    pub async fn bind(rt: &Runtime, addr: SocketAddr) -> io::Result<QuicUdp> {
        let sock = UdpSocket::bind(addr).await?;
        rt.add_native_link("udp", MTU, NativeTx::Done);
        rt.set_quic_classifier(true);
        Ok(QuicUdp { sock, peers: Vec::new(), clock: 0 })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    /// The token of `from`: slot index + 1 in the low 8 bits, the slot's generation above. A slot is only reused (least recently used first)
    /// with a new generation, so a token held by a module for an evicted peer never reaches the peer that took its place.
    fn token(&mut self, from: SocketAddr) -> u64 {
        self.clock += 1;
        let at = match self.peers.iter().position(|p| p.addr == Some(from)) {
            Some(at) => at,
            None if self.peers.len() < PEERS => {
                self.peers.push(Peer { addr: None, gen: 0, used: 0 });
                self.peers.len() - 1
            }
            None => (0..PEERS).min_by_key(|i| self.peers[*i].used).unwrap_or(0),
        };
        let p = &mut self.peers[at];
        if p.addr != Some(from) {
            p.addr = Some(from);
            p.gen = (p.gen + 1) & 0xff_ffff;
        }
        p.used = self.clock;
        u64::from(p.gen) << 8 | (at as u64 + 1)
    }

    fn peer(&self, token: u64) -> Option<SocketAddr> {
        let p = self.peers.get(((token & 0xff) as usize).checked_sub(1)?)?;
        (u64::from(p.gen) == token >> 8).then_some(p.addr).flatten()
    }

    /// Wait for one datagram and give it to the link.
    pub async fn recv(&mut self, rt: &Runtime) -> io::Result<()> {
        let mut buf = [0u8; 2048];
        let (n, from) = self.sock.recv_from(&mut buf).await?;
        let token = self.token(from);
        rt.inject_frame("udp", token, 0, &buf[..n]);
        Ok(())
    }

    /// Send what modules wrote to the link since the last call.
    pub async fn flush(&mut self, rt: &Runtime) {
        for (token, frame) in rt.take_link_tx("udp") {
            if let Some(to) = self.peer(token) {
                let _ = self.sock.send_to(&frame, to).await;
            }
        }
    }
}
