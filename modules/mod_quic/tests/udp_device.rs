//! Explicit device test: C Main + optional mod_quic, UDP port3339, trusted LAN.
//! No serial access and no dependency on local-only legacy tests/examples.
use quic_lite::{heap_packet_pool::HeapPacketPool, nostd::NoStdRuntime, *};
use spin::Mutex;
use std::{
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};
type Pool = HeapPacketPool<16, 1704>;
static POOL: Pool = Pool::new();
struct UdpBearer {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    context: Arc<Mutex<Option<BearerContext<Pool>>>>,
}
impl PacketEgress<<Pool as PacketPool>::Buffer> for UdpBearer {
    fn submit(
        &mut self,
        _: PeerL2Address,
        s: EgressSubmission<<Pool as PacketPool>::Buffer>,
    ) -> Result<(), PacketSubmitError<<Pool as PacketPool>::Buffer>> {
        match self.socket.send_to(s.packet().bytes(), self.peer) {
            Ok(n) if n == s.packet().bytes().len() => {
                s.complete(PacketSendOutcome::Sent, 0);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(PacketSubmitError::WouldBlock(s))
            }
            _ => {
                s.complete(PacketSendOutcome::Failed, 0);
                Ok(())
            }
        }
    }
}
impl PacketBearer<Pool> for UdpBearer {
    type AttachError = ();
    fn info(&self) -> BearerInfo {
        BearerInfo {
            name: BearerName::new("udp").unwrap(),
            max_packet_size: 1100,
            prefix_required: 144,
            suffix_required: 24,
            requires_packet_encryption: false,
            secure_link: false,
            nominal_bitrate_bps: 1_000_000,
            local_mac: None,
        }
    }
    fn attach(&mut self, c: BearerContext<Pool>) -> Result<(), ()> {
        *self.context.lock() = Some(c);
        Ok(())
    }
}
#[test]
#[ignore = "requires an explicitly configured trusted C-runtime device; XIP_QUIC_PEER=IP:3339"]
fn c_runtime_udp_echo_two_streams() {
    let peer: SocketAddr = std::env::var("XIP_QUIC_PEER")
        .expect("set XIP_QUIC_PEER=IP:3339")
        .parse()
        .unwrap();
    POOL.resize(16).unwrap();
    let socket = Arc::new(
        UdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .unwrap(),
    );
    socket.set_nonblocking(true).unwrap();
    let context = Arc::new(Mutex::new(None));
    let mut node: NoStdRuntime<_, 1100> = NoStdRuntime::new(
        QuicNode::new(&POOL),
        ConnectionLimits::with_receive_profile(4096, 2048, 2),
    );
    let bearer = node
        .add_bearer(UdpBearer {
            socket: socket.clone(),
            peer,
            context: context.clone(),
        })
        .unwrap();
    let address = |now| PacketMeta {
        bearer,
        peer_l2_address: PeerL2Address::new(1).unwrap(),
        received_at_us: now,
    };
    let association = node.associate(address(1), 1).unwrap();
    let payload = [vec![0x7e; 4096], vec![0x7d; 4096]];
    let mut sent = [0usize; 2];
    let mut received = [Vec::new(), Vec::new()];
    let mut streams = None;
    let mut ids = [0; 2];
    let started = Instant::now();
    let mut rx = [0u8; 1500];
    while started.elapsed() < Duration::from_secs(20) {
        let now = started.elapsed().as_micros() as u64 + 1;
        loop {
            match socket.recv_from(&mut rx) {
                Ok((n, source)) if source == peer => {
                    // OS socket memory is borrowed; acquire a pool lease at this boundary.
                    let packet = PacketPool::build_packet(&POOL, 144, 0, |out| {
                        out[..n].copy_from_slice(&rx[..n]);
                        Ok::<_, ()>(n)
                    })
                    .unwrap();
                    context
                        .lock()
                        .as_ref()
                        .unwrap()
                        .enqueue_packet(address(now), packet);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("receive: {e}"),
            }
        }
        context.lock().as_ref().unwrap().send_ready();
        node.advance_time(now).ok();
        node.progress_all().ok();
        if streams.is_none() && node.association_is_established(association) {
            let a = node.open_stream(association).unwrap();
            let b = node.open_stream(association).unwrap();
            ids = [a.id(), b.id()];
            streams = Some([a, b]);
        }
        if let Some(streams) = streams.as_mut() {
            for i in 0..2 {
                if sent[i] < payload[i].len() {
                    if let Ok(n) =
                        node.write_stream_and_finish(&mut streams[i], &payload[i][sent[i]..])
                    {
                        sent[i] += n;
                    }
                }
            }
        }
        while let Some(c) = node.next_stream_chunk() {
            let i = ids.iter().position(|id| *id == c.stream.id()).unwrap();
            assert_eq!(c.offset as usize, received[i].len());
            received[i].extend(c.bytes);
        }
        if received.iter().all(|p| p.len() == 4096) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        received.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![4096, 4096],
        "receiver echo incomplete; sent {sent:?}"
    );
    assert_eq!(received, payload);
    eprintln!(
        "C runtime receiver echoed {} bytes across two streams in {:?}",
        8192,
        started.elapsed()
    );
    drop(node);
    *context.lock() = None;
    POOL.resize(0).unwrap();
}
