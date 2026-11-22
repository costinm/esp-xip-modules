//! Live hardware integration and performance test across all 5 transport options on a real ESP32
//! board (such as the RISC-V `esp32c6`):
//! 1. Plain TCP (`--stream 3` for `mod_rtest` on port 5228, `--stream 2` for `mod_flash` on port 5227)
//! 2. Original `quic-lite` (`mod_quic` component `48`, over Wi-Fi UDP `:3339` and PPP serial `247`)
//! 3. `quic-mux` only (`mod_mux` component `52`, over Wi-Fi TCP `:5229` `--stream 4` and PPP serial `245`)
//! 4. `quic-link` standalone (`mod_qlink` component `54`, over Wi-Fi UDP `:3339` and PPP serial `242`)
//! 5. `quic-link` + `quic-mux` (`mod_quic2` component `55`, over Wi-Fi TCP `:5231` `--stream 6`, Wi-Fi UDP `:3339`, and PPP serial `243`)
//!
//! Opt-in via `XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...` (and optional `XIP_TRANSPORT_IP`).
//! Never hardcodes any board name, serial path, MAC, or IP address.

use dmesh_cli::{
    mux_cli::{
        MuxClientTarget, PPP_MUX_MTU, ProbeDirection, ProbeServiceRequest, TokioMuxConfig,
        connect_mux_client, mux_exchange, mux_probe,
    },
    run_dmesh_cli_args,
    xip_wire::{Event, Reader, XipPort, encode_record, unwrap_answer, wrap_request},
};
use quic_tools::{ProbeResponseReader, encode_probe_handler_request};
use quic_base::wire::StreamPacketPlan;
use quic_link::{
    AssocConfig, CongestionMode, ConnectionId, LinkFraming, LinkId, LinkProfile, LinkTxOutcome,
    QuicLinkAssoc, SingleStreamCallbacks, SingleStreamConfig, SingleStreamEndpoint,
    flow::{RxOutcome as LinkRxOutcome, StreamPriority},
    link::OWNER_ASSOC,
    pool::{
        PacketPool as LinkPacketPool, PoolPacket as LinkPoolPacket, StaticPacketLease,
        StaticPacketPool,
    },
    streams::{RxStreamChunk as LinkRxStreamChunk, TxStreamData as LinkTxStreamData},
    wire::plan_short_header_stream_packet,
};
use quic_tools::probe::ProbePayload;
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, QuicMux, RxMeta,
    RxOutcome as MuxRxOutcome, RxStreamChunk as MuxRxStreamChunk, StreamCloseReason, StreamError,
    StreamId, TxStreamData as MuxTxStreamData,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use xip_abi::cbor;

const PORT_QUIC: u32 = 247;
const PORT_QLINK: u32 = 242;
const PORT_QUIC2: u32 = 243;

fn encode_flash_write_header(name: &[u8], size: usize, req_id: u64) -> Vec<u8> {
    let mut fbuf = [0u8; 64];
    let mut fw = cbor::Writer::new(&mut fbuf);
    fw.map(2).uint(1).bytes(name).uint(2).uint(size as u64);
    let fields = fw.finish().unwrap();

    let mut ebuf = [0u8; 128];
    let mut ew = cbor::Writer::new(&mut ebuf);
    ew.map(4)
        .uint(1)
        .uint(44)
        .uint(2)
        .uint(4)
        .uint(3)
        .uint(req_id)
        .uint(5)
        .raw(fields);
    ew.finish().unwrap().to_vec()
}

fn encode_flash_read_header(name: &[u8], req_id: u64) -> Vec<u8> {
    let mut fbuf = [0u8; 64];
    let mut fw = cbor::Writer::new(&mut fbuf);
    fw.map(1).uint(1).bytes(name);
    let fields = fw.finish().unwrap();

    let mut ebuf = [0u8; 128];
    let mut ew = cbor::Writer::new(&mut ebuf);
    ew.map(4)
        .uint(1)
        .uint(44)
        .uint(2)
        .uint(3)
        .uint(3)
        .uint(req_id)
        .uint(5)
        .raw(fields);
    ew.finish().unwrap().to_vec()
}

fn split_flash_stream_response(resp: &[u8]) -> (&[u8], &[u8]) {
    let mut r = cbor::Reader::new(resp);
    assert!(
        r.skip(),
        "expected valid CBOR map header at start of flash stream response"
    );
    let body = r.rest();
    let hdr_len = resp.len() - body.len();
    (&resp[..hdr_len], body)
}

/// Send one native CBOR envelope (`0x444d0002` long packet on PPP port `247`) and return the raw
/// CBOR result bytes (`key 6`).
fn serial_native_call(
    serial_path: &str,
    component: u64,
    method: u64,
    fields_cbor: &[u8],
) -> Result<Vec<u8>, String> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(900_000);
    let req_id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut ebuf = [0u8; 256];
    let mut ew = cbor::Writer::new(&mut ebuf);
    ew.map(4)
        .uint(1)
        .uint(component)
        .uint(2)
        .uint(method)
        .uint(3)
        .uint(req_id)
        .uint(5)
        .raw(fields_cbor);
    let env = ew.finish().ok_or("request envelope too large")?;
    let wire = encode_record(PORT_QUIC, &wrap_request(env));

    let mut port = XipPort::open(serial_path.to_owned()).map_err(|e| e.to_string())?;
    port.wake()?;
    let mut reader = Reader::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut next_tx = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_tx {
            port.write_all(&wire).map_err(|e| e.to_string())?;
            next_tx = Instant::now() + Duration::from_millis(400);
        }
        let bytes = port.read().map_err(|e| e.to_string())?;
        for ev in reader.feed(&bytes) {
            if let Event::Record(PORT_QUIC, pkt) = ev {
                let Some(ans_env) = unwrap_answer(&pkt) else {
                    continue;
                };
                if cbor::get_int(ans_env, 1) != Ok(Some(component as i64))
                    || cbor::get_int(ans_env, 2) != Ok(Some(method as i64))
                    || cbor::get_int(ans_env, 3) != Ok(Some(req_id as i64))
                {
                    continue;
                }
                if let Ok(Some(err)) = cbor::get_int(ans_env, 7) {
                    return Err(format!("native {component}.{method} error {err}"));
                }
                if let Ok(Some(mut r)) = cbor::get(ans_env, 6) {
                    let rest = r.rest();
                    if r.skip() {
                        let used = rest.len() - r.rest().len();
                        return Ok(rest[..used].to_vec());
                    }
                }
            }
        }
        let _ = reader.take_text();
        if bytes.is_empty() {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    Err(format!("timeout waiting for native {component}.{method}"))
}

fn wait_module_running(serial_path: &str, name: &[u8], want_running: bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let status = serial_native_call(serial_path, 49, 5, &[0xa0])?;
        let mut found_running = None;
        if let Ok(Some(mut mods_r)) = cbor::get(&status, 4) {
            if let Some(cbor::Item::Array(count)) = mods_r.next() {
                for _ in 0..count {
                    let slot_rest = mods_r.rest();
                    if !mods_r.skip() {
                        break;
                    }
                    let slot_map = &slot_rest[..slot_rest.len() - mods_r.rest().len()];
                    if cbor::get_text(slot_map, 1) == Ok(Some(name)) {
                        let running = match cbor::get(slot_map, 4).ok().flatten().and_then(|mut r| r.next()) {
                            Some(cbor::Item::Bool(b)) => b,
                            _ => false,
                        };
                        found_running = Some(running);
                    }
                }
            }
        }
        if found_running == Some(want_running) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Err(format!(
        "timeout waiting for module {:?} running={want_running}",
        std::str::from_utf8(name).unwrap_or("?")
    ))
}

fn sys_start(serial_path: &str, name: &str, request: &[u8]) -> Result<(), String> {
    let mut fbuf = [0u8; 128];
    let mut fw = cbor::Writer::new(&mut fbuf);
    if request.is_empty() {
        fw.map(1).uint(1).text(name);
    } else {
        fw.map(2).uint(1).text(name).uint(2).bytes(request);
    }
    let fields = fw.finish().unwrap();
    match serial_native_call(serial_path, 49, 6, fields) {
        Ok(_) => {}
        Err(e) if e.contains("error -103") => {} // ERR_BUSY = already running
        Err(e) => return Err(e),
    }
    wait_module_running(serial_path, name.as_bytes(), true)?;
    if name == "test" && request == b"probe" {
        // Trigger `HOST_START` synchronously by querying `rtest.info` (`2.2`) so `stream.listen(8, 2)`
        // and the 5ms tick alarm are initialized before the first TCP or stream request arrives.
        let _ = serial_native_call(serial_path, 2, 2, &[0xa0]);
    }
    std::thread::sleep(Duration::from_millis(20));
    Ok(())
}

fn sys_stop(serial_path: &str, name: &str) -> Result<(), String> {
    let mut fbuf = [0u8; 64];
    let mut fw = cbor::Writer::new(&mut fbuf);
    fw.map(1).uint(1).text(name);
    let fields = fw.finish().unwrap();
    match serial_native_call(serial_path, 49, 7, fields) {
        Ok(_) => wait_module_running(serial_path, name.as_bytes(), false),
        Err(e) if e.contains("error -7") => Ok(()), // ERR_NOTFOUND = already stopped
        Err(e) => Err(e),
    }
}

fn discover_device_ip(serial_path: &str) -> Result<IpAddr, String> {
    if let Ok(ip_str) = std::env::var("XIP_TRANSPORT_IP") {
        if !ip_str.trim().is_empty() {
            return ip_str
                .trim()
                .parse::<IpAddr>()
                .map_err(|e| format!("invalid XIP_TRANSPORT_IP: {e}"));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let status = serial_native_call(serial_path, 49, 5, &[0xa0])?;
        if let Ok(Some(mut wifi_r)) = cbor::get(&status, 2) {
            let wifi_rest = wifi_r.rest();
            if wifi_r.skip() {
                let wifi_map = &wifi_rest[..wifi_rest.len() - wifi_r.rest().len()];
                if let Ok(Some(ip_bytes)) = cbor::get_text(wifi_map, 3) {
                    if let Ok(ip_str) = std::str::from_utf8(ip_bytes) {
                        if let Ok(ip) = ip_str.parse::<IpAddr>() {
                            if !ip.is_unspecified() {
                                return Ok(ip);
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("device STA has no IPv4 address in sys.status; join Wi-Fi or set XIP_TRANSPORT_IP".into())
}

#[derive(Clone, Debug)]
struct TransferStats {
    elapsed_us: u64,
    tx_packets: u64,
    rx_packets: u64,
}

impl TransferStats {
    fn kib_per_sec(&self, bytes: u64) -> f64 {
        if self.elapsed_us == 0 {
            0.0
        } else {
            (bytes as f64 / 1024.0) / (self.elapsed_us as f64 / 1_000_000.0)
        }
    }
    fn mbps(&self, bytes: u64) -> f64 {
        if self.elapsed_us == 0 {
            0.0
        } else {
            (bytes as f64 * 8.0) / (self.elapsed_us as f64)
        }
    }
}

#[derive(Clone, Debug)]
struct PerfRow {
    transport: &'static str,
    bearer: &'static str,
    operation: &'static str,
    bytes: u64,
    stats: TransferStats,
}

// ============================================================================
// Live Device Driver 1: `mod_qlink` (`SingleStreamEndpoint` over UDP `:3339` or PPP `242`)
// ============================================================================

#[derive(Default)]
struct QlinkClientCollector {
    bytes: Vec<u8>,
    fin: bool,
}

impl<P: LinkPoolPacket> SingleStreamCallbacks<P> for QlinkClientCollector {
    fn on_stream_data(&mut self, chunk: LinkRxStreamChunk<'_, P>, fin: bool) -> LinkRxOutcome<P> {
        self.bytes.extend_from_slice(chunk.bytes());
        if fin {
            self.fin = true;
        }
        LinkRxOutcome::Consumed(chunk.len())
    }
}

enum PacketIo {
    Udp(UdpSocket),
    Serial {
        port: XipPort,
        ppp_port: u32,
        reader: Reader,
    },
}

impl PacketIo {
    fn new_udp(peer: SocketAddr) -> Self {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind UDP client");
        sock.connect(peer).expect("connect UDP client");
        sock.set_nonblocking(true).expect("set_nonblocking UDP");
        Self::Udp(sock)
    }

    fn new_serial(serial_path: &str, ppp_port: u32) -> Self {
        let port = XipPort::open(serial_path.to_owned()).expect("open serial port");
        Self::Serial {
            port,
            ppp_port,
            reader: Reader::default(),
        }
    }

    fn send_packet(&mut self, bytes: &[u8]) {
        match self {
            Self::Udp(sock) => {
                let _ = sock.send(bytes);
            }
            Self::Serial { port, ppp_port, .. } => {
                let wire = encode_record(*ppp_port, bytes);
                port.write_all(&wire).expect("write PPP record");
            }
        }
    }

    fn recv_packets(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Udp(sock) => {
                let mut buf = [0u8; 2048];
                while let Ok(n) = sock.recv(&mut buf) {
                    if n > 0 {
                        out.push(buf[..n].to_vec());
                    }
                }
            }
            Self::Serial {
                port,
                ppp_port,
                reader,
            } => {
                if let Ok(bytes) = port.read() {
                    if !bytes.is_empty() {
                        for ev in reader.feed(&bytes) {
                            if let Event::Record(p, data) = ev {
                                if p == *ppp_port {
                                    out.push(data);
                                }
                            }
                        }
                        let _ = reader.take_text();
                    }
                }
            }
        }
        out
    }
}

struct QlinkDeviceClient {
    endpoint: SingleStreamEndpoint<&'static StaticPacketPool<32>>,
    link_id: LinkId,
    io: PacketIo,
    next_stream_idx: u64,
    start_instant: Instant,
}

impl QlinkDeviceClient {
    fn new(io: PacketIo, mtu: usize, is_serial: bool) -> Self {
        let pool_ref: &'static StaticPacketPool<32> =
            Box::leak(Box::new(StaticPacketPool::<32>::new()));
        let cid = ConnectionId::new(1).unwrap();
        let cfg = SingleStreamConfig {
            stream_id: StreamId(0),
            stream_priority: StreamPriority::NORMAL,
            initial_recv_window: 64 * 1024,
            initial_send_window: 64 * 1024,
            max_reassembly_chunks: 8,
            assoc: AssocConfig {
                assoc_id: OWNER_ASSOC,
                local_cid: None,
                default_peer_cid: cid,
                max_retained_tx_packets: if is_serial { 6 } else { 8 },
                ack_eliciting_threshold: 2,
                max_ack_delay_us: 15_000,
                ..AssocConfig::default()
            },
        };
        let mut endpoint = SingleStreamEndpoint::new(pool_ref, cfg);
        let link_id = LinkId(1);
        endpoint.assoc_mut().upsert_link(
            link_id,
            LinkProfile {
                tier: 0,
                mtu,
                max_priority: StreamPriority::BULK,
                congestion_mode: CongestionMode::FlowControlled {
                    max_inflight_packets: 4,
                    max_inflight_bytes: if is_serial { 4 * 1024 } else { 6 * 1024 },
                },
                framing: LinkFraming::Datagram { peer_cid: cid },
                initial_rtt_us: if is_serial { 120_000 } else { 20_000 },
                encrypt: false,
            },
        );
        Self {
            endpoint,
            link_id,
            io,
            next_stream_idx: 0,
            start_instant: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start_instant.elapsed().as_micros() as u64 + 1_000
    }

    fn execute_stream(
        &mut self,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        let sid = StreamId(self.next_stream_idx * 4);
        self.next_stream_idx += 1;
        if sid.0 != 0 {
            self.endpoint.reset_stream_state(sid);
        }

        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1024];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;
        let mut collector = QlinkClientCollector::default();

        while Instant::now() < deadline {
            let now_us = self.now_us();

            while !tx_fin_sent {
                let max_w = self.endpoint.max_writable_payload_len();
                if max_w == 0 {
                    break;
                }
                if hdr_at < header.len() {
                    let has_more = upload_payload.is_some() || !raw_body.is_empty();
                    let n = (header.len() - hdr_at).min(max_w);
                    let fin = !has_more && (hdr_at + n == header.len());
                    match self.endpoint.write_stream(
                        LinkTxStreamData::Bytes(&header[hdr_at..hdr_at + n]),
                        fin,
                        now_us,
                    ) {
                        Ok(written) if written > 0 => {
                            hdr_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if let Some(payload) = upload_payload.as_mut() {
                    if chunk_at == chunk_len {
                        if payload.is_complete() {
                            if self
                                .endpoint
                                .write_stream(LinkTxStreamData::Bytes(&[]), true, now_us)
                                .is_ok()
                            {
                                tx_fin_sent = true;
                            }
                            break;
                        }
                        chunk_len = payload.fill(&mut chunk_buf);
                        chunk_at = 0;
                    }
                    let rem = chunk_len - chunk_at;
                    let n = rem.min(max_w);
                    let fin = payload.is_complete() && (chunk_at + n == chunk_len);
                    match self.endpoint.write_stream(
                        LinkTxStreamData::Bytes(&chunk_buf[chunk_at..chunk_at + n]),
                        fin,
                        now_us,
                    ) {
                        Ok(written) if written > 0 => {
                            chunk_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if raw_at < raw_body.len() {
                    let n = (raw_body.len() - raw_at).min(max_w);
                    let fin = raw_at + n == raw_body.len();
                    match self.endpoint.write_stream(
                        LinkTxStreamData::Bytes(&raw_body[raw_at..raw_at + n]),
                        fin,
                        now_us,
                    ) {
                        Ok(written) if written > 0 => {
                            raw_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else {
                    if self
                        .endpoint
                        .write_stream(LinkTxStreamData::Bytes(&[]), true, now_us)
                        .is_ok()
                    {
                        tx_fin_sent = true;
                    }
                    break;
                }
            }

            let mut moved = false;
            while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
                self.io.send_packet(out.packet.bytes());
                self.endpoint.assoc_mut().on_driver_complete(
                    self.link_id,
                    out.packet_number,
                    LinkTxOutcome::Sent,
                    now_us,
                );
                tx_packets += 1;
                moved = true;
            }

            let replies = self.io.recv_packets();
            if !replies.is_empty() {
                moved = true;
                let now_rx = self.now_us();
                for wire in replies {
                    rx_packets += 1;
                    let Ok(mut p) = self.endpoint.alloc_packet() else {
                        break;
                    };
                    p.append(&wire).unwrap();
                    let _ = self
                        .endpoint
                        .receive_packet(self.link_id, p, now_rx, &mut collector);
                }
                if let Some(reader) = probe_reader.as_deref_mut() {
                    if !collector.bytes.is_empty() {
                        reader
                            .push(&collector.bytes)
                            .expect("valid probe response chunk");
                        collector.bytes.clear();
                    }
                }
            }

            let now_flush = self.now_us();
            let _ = self.endpoint.poll_timers(now_flush);
            while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
                self.io.send_packet(out.packet.bytes());
                self.endpoint.assoc_mut().on_driver_complete(
                    self.link_id,
                    out.packet_number,
                    LinkTxOutcome::Sent,
                    now_flush,
                );
                tx_packets += 1;
                moved = true;
            }

            if tx_fin_sent && collector.fin {
                break;
            }
            if !moved {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(
            tx_fin_sent && collector.fin,
            "QlinkDeviceClient stream exchange timed out (tx_fin={tx_fin_sent}, rx_fin={})",
            collector.fin
        );

        // Flush final ACK so device releases retained SACK packets.
        let ack_ts = self.now_us() + 50_000;
        let _ = self.endpoint.poll_timers(ack_ts);
        while let Some(ack) = self.endpoint.assoc_mut().pop_outbound_packet() {
            self.io.send_packet(ack.packet.bytes());
            self.endpoint.assoc_mut().on_driver_complete(
                self.link_id,
                ack.packet_number,
                LinkTxOutcome::Sent,
                ack_ts,
            );
            tx_packets += 1;
        }
        std::thread::sleep(Duration::from_millis(15));
        let _ = self.io.recv_packets();

        if let Some(reader) = probe_reader {
            if !collector.bytes.is_empty() {
                reader
                    .push(&collector.bytes)
                    .expect("valid final probe response");
                collector.bytes.clear();
            }
            assert!(reader.is_complete(), "probe reader incomplete at stream FIN");
        }

        let elapsed_us = started.elapsed().as_micros() as u64;
        (
            collector.bytes,
            TransferStats {
                elapsed_us,
                tx_packets,
                rx_packets,
            },
        )
    }
}

// ============================================================================
// Live Device Driver 2: `mod_quic2` (`QuicLinkAssoc` + `QuicMux` over UDP, PPP `243`, or TCP `:5231`)
// ============================================================================

#[derive(Default)]
struct Quic2ClientRx {
    streams: BTreeMap<StreamId, (Vec<u8>, bool)>,
}

impl<P: LinkPoolPacket> MuxCallbacks<P> for Quic2ClientRx {
    fn on_stream_data(
        &mut self,
        _assoc: AssociationId,
        stream_id: StreamId,
        _offset: u64,
        data: MuxRxStreamChunk<'_, P>,
        fin: bool,
    ) -> Result<MuxRxOutcome<P>, StreamError> {
        let entry = self.streams.entry(stream_id).or_default();
        entry.0.extend_from_slice(data.bytes());
        if fin {
            entry.1 = true;
        }
        Ok(MuxRxOutcome::Consumed(data.len()))
    }
    fn on_stream_writable(&mut self, _: AssociationId, _: StreamId, _: usize) {}
    fn on_association_writable(&mut self, _: AssociationId, _: u64) {}
    fn on_device_writable(&mut self) {}
    fn on_stream_closed(&mut self, _: AssociationId, _: StreamId, _: StreamCloseReason) {}
    fn on_association_broken(&mut self, _: AssociationId, _: AssociationBrokenReason) {}
}

struct Quic2DatagramDeviceClient {
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    io: PacketIo,
    start_instant: Instant,
}

impl Quic2DatagramDeviceClient {
    fn new(io: PacketIo, mtu: usize, is_serial: bool) -> Self {
        let pool_ref: &'static StaticPacketPool<32> =
            Box::leak(Box::new(StaticPacketPool::<32>::new()));
        let limits = MuxLimits {
            initial_max_data: 64 * 1024,
            initial_max_stream_data: 32 * 1024,
            initial_max_streams_bidi: 8,
            initial_max_streams_uni: 8,
            max_concurrent_streams: 8,
            max_queued_chunks_per_stream: 8,
            max_association_rx_packets: 12,
            max_device_rx_packets: 16,
            gap_timeout_us: 5_000_000,
        };
        let mut mux = QuicMux::new(pool_ref, limits);
        mux.add_association(OWNER_ASSOC, true).unwrap();

        let cid = ConnectionId::new(1).unwrap();
        let mut assoc = QuicLinkAssoc::new(AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: None,
            default_peer_cid: cid,
            max_retained_tx_packets: if is_serial { 6 } else { 8 },
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 15_000,
            ..AssocConfig::default()
        });
        let link_id = LinkId(1);
        assoc.upsert_link(
            link_id,
            LinkProfile {
                tier: 0,
                mtu,
                max_priority: StreamPriority::BULK,
                congestion_mode: CongestionMode::FlowControlled {
                    max_inflight_packets: 4,
                    max_inflight_bytes: if is_serial { 4 * 1024 } else { 6 * 1024 },
                },
                framing: LinkFraming::Datagram { peer_cid: cid },
                initial_rtt_us: if is_serial { 120_000 } else { 20_000 },
                encrypt: false,
            },
        );
        Self {
            pool_ref,
            mux,
            assoc,
            link_id,
            io,
            start_instant: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start_instant.elapsed().as_micros() as u64 + 1_000
    }

    fn execute_stream(
        &mut self,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        let pool_ref = self.pool_ref;
        let stream_id = self.mux.open_stream(OWNER_ASSOC, true).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1024];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;
        let mut rx_cb = Quic2ClientRx::default();

        while Instant::now() < deadline {
            let now_us = self.now_us();

            while !tx_fin_sent {
                if !self.assoc.can_admit_priority(StreamPriority::NORMAL) {
                    break;
                }
                let Ok(cap) = self
                    .mux
                    .stream_send_capacity(OWNER_ASSOC, stream_id, &self.assoc)
                else {
                    break;
                };
                if !cap.can_send_now || cap.max_chunk_bytes == 0 {
                    break;
                }
                let max_w = cap.max_chunk_bytes;
                if hdr_at < header.len() {
                    let has_more = upload_payload.is_some() || !raw_body.is_empty();
                    let n = (header.len() - hdr_at).min(max_w);
                    let fin = !has_more && (hdr_at + n == header.len());
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&header[hdr_at..hdr_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            hdr_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if let Some(payload) = upload_payload.as_mut() {
                    if chunk_at == chunk_len {
                        if payload.is_complete() {
                            if self
                                .mux
                                .write_stream(
                                    OWNER_ASSOC,
                                    stream_id,
                                    MuxTxStreamData::Bytes(&[]),
                                    true,
                                    &mut self.assoc,
                                )
                                .is_ok()
                            {
                                tx_fin_sent = true;
                            }
                            break;
                        }
                        chunk_len = payload.fill(&mut chunk_buf);
                        chunk_at = 0;
                    }
                    let rem = chunk_len - chunk_at;
                    let n = rem.min(max_w);
                    let fin = payload.is_complete() && (chunk_at + n == chunk_len);
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&chunk_buf[chunk_at..chunk_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            chunk_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if raw_at < raw_body.len() {
                    let n = (raw_body.len() - raw_at).min(max_w);
                    let fin = raw_at + n == raw_body.len();
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&raw_body[raw_at..raw_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            raw_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else {
                    if self
                        .mux
                        .write_stream(
                            OWNER_ASSOC,
                            stream_id,
                            MuxTxStreamData::Bytes(&[]),
                            true,
                            &mut self.assoc,
                        )
                        .is_ok()
                    {
                        tx_fin_sent = true;
                    }
                    break;
                }
            }

            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);

            let mut moved = false;
            while let Some(out) = self.assoc.pop_outbound_packet() {
                self.io.send_packet(out.packet.bytes());
                self.assoc.on_driver_complete(
                    self.link_id,
                    out.packet_number,
                    LinkTxOutcome::Sent,
                    now_us,
                );
                tx_packets += 1;
                moved = true;
            }

            let replies = self.io.recv_packets();
            if !replies.is_empty() {
                moved = true;
                let now_rx = self.now_us();
                for wire in replies {
                    rx_packets += 1;
                    let Ok(mut p) = pool_ref.alloc() else {
                        break;
                    };
                    p.append(&wire).unwrap();
                    if let Ok(demux) = self.assoc.receive_packet(self.link_id, p, now_rx) {
                        if let Some(mux_pkt) = demux.mux_packet {
                            let _ = self.mux.receive_packet(
                                RxMeta {
                                    association: OWNER_ASSOC,
                                    received_at_us: now_rx,
                                },
                                mux_pkt,
                                &mut self.assoc,
                                &mut rx_cb,
                            );
                        }
                    }
                }
                if let Some(reader) = probe_reader.as_deref_mut() {
                    if let Some((buf, _)) = rx_cb.streams.get_mut(&stream_id) {
                        if !buf.is_empty() {
                            reader.push(buf).expect("valid probe response chunk");
                            buf.clear();
                        }
                    }
                }
            }

            let now_flush = self.now_us();
            let mut pr = pool_ref;
            let _ = self.assoc.poll_timers(&mut pr, now_flush);
            let _ = self
                .mux
                .advance_time(now_flush, &mut self.assoc, &mut rx_cb);
            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
            while let Some(out) = self.assoc.pop_outbound_packet() {
                self.io.send_packet(out.packet.bytes());
                self.assoc.on_driver_complete(
                    self.link_id,
                    out.packet_number,
                    LinkTxOutcome::Sent,
                    now_flush,
                );
                tx_packets += 1;
                moved = true;
            }

            let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
            if tx_fin_sent && rx_fin {
                break;
            }
            if !moved {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
        assert!(
            tx_fin_sent && rx_fin,
            "Quic2DatagramDeviceClient stream exchange timed out (tx_fin={tx_fin_sent}, rx_fin={rx_fin})"
        );

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        let ack_ts = self.now_us() + 50_000;
        let mut pr = pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, ack_ts);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            self.io.send_packet(ack.packet.bytes());
            self.assoc.on_driver_complete(
                self.link_id,
                ack.packet_number,
                LinkTxOutcome::Sent,
                ack_ts,
            );
            tx_packets += 1;
        }
        std::thread::sleep(Duration::from_millis(15));
        let _ = self.io.recv_packets();

        let (rem_bytes, _) = rx_cb.streams.remove(&stream_id).unwrap_or_default();
        if let Some(reader) = probe_reader {
            if !rem_bytes.is_empty() {
                reader.push(&rem_bytes).expect("valid final probe response");
            }
            assert!(reader.is_complete(), "probe reader incomplete at stream FIN");
        }

        let elapsed_us = started.elapsed().as_micros() as u64;
        (
            rem_bytes,
            TransferStats {
                elapsed_us,
                tx_packets,
                rx_packets,
            },
        )
    }
}

struct Quic2TcpDeviceClient {
    tcp: TcpStream,
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    rx_wire_buf: Vec<u8>,
    start_instant: Instant,
}

impl Quic2TcpDeviceClient {
    async fn connect(tcp_addr: SocketAddr) -> Self {
        let tcp = TcpStream::connect(tcp_addr)
            .await
            .expect("connect mod_quic2 TCP");
        let _ = tcp.set_nodelay(true);

        let pool_ref: &'static StaticPacketPool<32> =
            Box::leak(Box::new(StaticPacketPool::<32>::new()));
        let limits = MuxLimits {
            initial_max_data: 64 * 1024,
            initial_max_stream_data: 32 * 1024,
            initial_max_streams_bidi: 8,
            initial_max_streams_uni: 8,
            max_concurrent_streams: 8,
            max_queued_chunks_per_stream: 8,
            max_association_rx_packets: 12,
            max_device_rx_packets: 16,
            gap_timeout_us: 5_000_000,
        };
        let mut mux = QuicMux::new(pool_ref, limits);
        mux.add_association(OWNER_ASSOC, true).unwrap();

        let cid = ConnectionId::new(1).unwrap();
        let mut assoc = QuicLinkAssoc::new(AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: None,
            default_peer_cid: cid,
            max_retained_tx_packets: 8,
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 15_000,
            ..AssocConfig::default()
        });
        let link_id = LinkId(10);
        assoc.upsert_link(
            link_id,
            LinkProfile {
                tier: 0,
                mtu: 1500,
                max_priority: StreamPriority::BULK,
                congestion_mode: CongestionMode::FlowControlled {
                    max_inflight_packets: 6,
                    max_inflight_bytes: 9 * 1024,
                },
                framing: LinkFraming::SelfDelimitingStream { peer_cid: cid },
                initial_rtt_us: 10_000,
                encrypt: false,
            },
        );

        Self {
            tcp,
            pool_ref,
            mux,
            assoc,
            link_id,
            rx_wire_buf: Vec::with_capacity(4096),
            start_instant: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start_instant.elapsed().as_micros() as u64 + 1_000
    }

    async fn execute_stream(
        &mut self,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        let stream_id = self.mux.open_stream(OWNER_ASSOC, true).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1024];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;

        let mut rx_cb = Quic2ClientRx::default();
        let mut read_tmp = [0u8; 2048];

        while Instant::now() < deadline {
            let now_us = self.now_us();
            while !tx_fin_sent {
                if !self.assoc.can_admit_priority(StreamPriority::NORMAL) {
                    break;
                }
                let Ok(cap) = self
                    .mux
                    .stream_send_capacity(OWNER_ASSOC, stream_id, &self.assoc)
                else {
                    break;
                };
                if !cap.can_send_now || cap.max_chunk_bytes == 0 {
                    break;
                }
                let max_w = cap.max_chunk_bytes;
                if hdr_at < header.len() {
                    let has_more = upload_payload.is_some() || !raw_body.is_empty();
                    let n = (header.len() - hdr_at).min(max_w);
                    let fin = !has_more && (hdr_at + n == header.len());
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&header[hdr_at..hdr_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            hdr_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if let Some(payload) = upload_payload.as_mut() {
                    if chunk_at == chunk_len {
                        if payload.is_complete() {
                            if self
                                .mux
                                .write_stream(
                                    OWNER_ASSOC,
                                    stream_id,
                                    MuxTxStreamData::Bytes(&[]),
                                    true,
                                    &mut self.assoc,
                                )
                                .is_ok()
                            {
                                tx_fin_sent = true;
                            }
                            break;
                        }
                        chunk_len = payload.fill(&mut chunk_buf);
                        chunk_at = 0;
                    }
                    let rem = chunk_len - chunk_at;
                    let n = rem.min(max_w);
                    let fin = payload.is_complete() && (chunk_at + n == chunk_len);
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&chunk_buf[chunk_at..chunk_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            chunk_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else if raw_at < raw_body.len() {
                    let n = (raw_body.len() - raw_at).min(max_w);
                    let fin = raw_at + n == raw_body.len();
                    match self.mux.write_stream(
                        OWNER_ASSOC,
                        stream_id,
                        MuxTxStreamData::Bytes(&raw_body[raw_at..raw_at + n]),
                        fin,
                        &mut self.assoc,
                    ) {
                        Ok(written) if written > 0 => {
                            raw_at += written;
                            if fin && written == n {
                                tx_fin_sent = true;
                            }
                        }
                        _ => break,
                    }
                } else {
                    if self
                        .mux
                        .write_stream(
                            OWNER_ASSOC,
                            stream_id,
                            MuxTxStreamData::Bytes(&[]),
                            true,
                            &mut self.assoc,
                        )
                        .is_ok()
                    {
                        tx_fin_sent = true;
                    }
                    break;
                }
            }

            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
            while let Some(out) = self.assoc.pop_outbound_packet() {
                self.tcp.write_all(out.packet.bytes()).await.unwrap();
                self.assoc.on_driver_complete(
                    self.link_id,
                    out.packet_number,
                    LinkTxOutcome::Sent,
                    now_us,
                );
                tx_packets += 1;
            }

            let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
            if tx_fin_sent && rx_fin {
                break;
            }

            match self.tcp.try_read(&mut read_tmp) {
                Ok(n) if n > 0 => {
                    let now_rx = self.now_us();
                    self.rx_wire_buf.extend_from_slice(&read_tmp[..n]);
                    loop {
                        match plan_short_header_stream_packet(&self.rx_wire_buf, 1500) {
                            Ok(StreamPacketPlan::Ready { total_len }) => {
                                if self.rx_wire_buf.len() < total_len {
                                    break;
                                }
                                rx_packets += 1;
                                let Ok(mut p) = self.pool_ref.alloc() else {
                                    break;
                                };
                                p.append(&self.rx_wire_buf[..total_len]).unwrap();
                                self.rx_wire_buf.drain(..total_len);
                                let demux = self
                                    .assoc
                                    .receive_packet(self.link_id, p, now_rx)
                                    .unwrap();
                                if let Some(mux_pkt) = demux.mux_packet {
                                    self.mux
                                        .receive_packet(
                                            RxMeta {
                                                association: OWNER_ASSOC,
                                                received_at_us: now_rx,
                                            },
                                            mux_pkt,
                                            &mut self.assoc,
                                            &mut rx_cb,
                                        )
                                        .unwrap();
                                }
                            }
                            Ok(StreamPacketPlan::NeedMoreHeader) => break,
                            Err(e) => {
                                panic!("invalid self-delimiting packet on mod_quic2 TCP: {e:?}")
                            }
                        }
                    }
                    if let Some(reader) = probe_reader.as_deref_mut() {
                        if let Some((buf, _)) = rx_cb.streams.get_mut(&stream_id) {
                            if !buf.is_empty() {
                                reader.push(buf).expect("valid probe response chunk");
                                buf.clear();
                            }
                        }
                    }
                    let mut pr = self.pool_ref;
                    let _ = self.assoc.poll_timers(&mut pr, now_rx);
                    let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
                }
                _ => {
                    let now_idle = self.now_us();
                    let mut pr = self.pool_ref;
                    let _ = self.assoc.poll_timers(&mut pr, now_idle);
                    let _ = self
                        .mux
                        .advance_time(now_idle, &mut self.assoc, &mut rx_cb);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
        let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
        assert!(
            tx_fin_sent && rx_fin,
            "Quic2TcpDeviceClient stream exchange timed out"
        );

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        let ack_ts = self.now_us() + 50_000;
        let mut pr = self.pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, ack_ts);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            let _ = self.tcp.write_all(ack.packet.bytes()).await;
            self.assoc.on_driver_complete(
                self.link_id,
                ack.packet_number,
                LinkTxOutcome::Sent,
                ack_ts,
            );
            tx_packets += 1;
        }

        let (rem_bytes, _) = rx_cb.streams.remove(&stream_id).unwrap_or_default();
        if let Some(reader) = probe_reader {
            if !rem_bytes.is_empty() {
                reader.push(&rem_bytes).expect("valid final probe response");
            }
            assert!(reader.is_complete(), "probe reader incomplete at stream FIN");
        }

        let elapsed_us = started.elapsed().as_micros() as u64;
        (
            rem_bytes,
            TransferStats {
                elapsed_us,
                tx_packets,
                rx_packets,
            },
        )
    }

    async fn close(mut self) {
        let _ = self.tcp.shutdown().await;
        drop(self.tcp);
    }
}

struct DeviceTestRestoreGuard {
    serial_path: String,
}

impl Drop for DeviceTestRestoreGuard {
    fn drop(&mut self) {
        let _ = sys_stop(&self.serial_path, "mux");
        let _ = sys_stop(&self.serial_path, "qlink");
        let _ = sys_stop(&self.serial_path, "quic2");
        let _ = sys_stop(&self.serial_path, "test");
        let _ = sys_start(&self.serial_path, "quic", &[]);
        let _ = sys_start(&self.serial_path, "nan", &[]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_device_all_5_transports_verification_and_benchmarks() {
    let Ok(serial_path) = std::env::var("XIP_TRANSPORT_DEVICE") else {
        eprintln!("skipping live device transport test: set XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...");
        return;
    };
    let _restore = DeviceTestRestoreGuard {
        serial_path: serial_path.clone(),
    };

    // 1. Ensure `nan` is running with sleepy disabled and a 300s awake hold so Wi-Fi STA stays
    //    associated throughout the benchmark, stop transport modules so only one owns `hw.quic`
    //    at a time, and start `test` (`mod_rtest`) in `probe` mode.
    let _ = sys_start(&serial_path, "nan", &[]);
    let mut sleepy_buf = [0u8; 32];
    let mut sw = cbor::Writer::new(&mut sleepy_buf);
    sw.map(1).uint(1).boolean(false);
    let _ = serial_native_call(&serial_path, 46, 6, sw.finish().unwrap());
    let mut awake_buf = [0u8; 32];
    let mut aw = cbor::Writer::new(&mut awake_buf);
    aw.map(1).uint(1).uint(300_000);
    let _ = serial_native_call(&serial_path, 46, 10, aw.finish().unwrap());

    sys_stop(&serial_path, "mux").expect("stop mux");
    sys_stop(&serial_path, "qlink").expect("stop qlink");
    sys_stop(&serial_path, "quic2").expect("stop quic2");
    sys_start(&serial_path, "quic", &[]).expect("start quic");
    sys_stop(&serial_path, "test").expect("stop test");
    sys_start(&serial_path, "test", b"probe").expect("start test in probe mode");

    let device_ip = discover_device_ip(&serial_path).expect("discover device IPv4");
    eprintln!("=== Live Device Transport Benchmark on {serial_path} (IP {device_ip}) ===");

    let payload_1k: Vec<u8> = (0..1024).map(|i| ((i * 37 + 11) & 0xff) as u8).collect();
    let wifi_sizes: [(u64, &'static str, &'static str); 3] = [
        (
            1024,
            "rtest.probe upload (1 KiB)",
            "rtest.probe download (1 KiB)",
        ),
        (
            64 * 1024,
            "rtest.probe upload (64 KiB)",
            "rtest.probe download (64 KiB)",
        ),
        (
            256 * 1024,
            "rtest.probe upload (256 KiB)",
            "rtest.probe download (256 KiB)",
        ),
    ];
    let serial_sizes: [(u64, &'static str, &'static str); 2] = [
        (
            1024,
            "rtest.probe upload (1 KiB)",
            "rtest.probe download (1 KiB)",
        ),
        (
            16 * 1024,
            "rtest.probe upload (16 KiB)",
            "rtest.probe download (16 KiB)",
        ),
    ];

    let mut rows: Vec<PerfRow> = Vec::new();

    // ========================================================================
    // Transport 1: Plain TCP (`mod_rtest` on `:5228`, `mod_flash` on `:5227`)
    // ========================================================================
    {
        let rtest_addr = SocketAddr::new(device_ip, 5228);
        let flash_addr = SocketAddr::new(device_ip, 5227);
        let mut req_id = 1000u64;

        for (sz, up_label, down_label) in wifi_sizes {
            for (dir, label) in [
                (ProbeDirection::Upload, up_label),
                (ProbeDirection::Download, down_label),
            ] {
                req_id += 1;
                let mut req = ProbeServiceRequest::new(sz, 1024);
                req.direction = dir;
                req.verify = true;
                let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                let t0 = Instant::now();
                let mut sock = TcpStream::connect(rtest_addr)
                    .await
                    .expect("connect Plain TCP :5228");
                let _ = sock.set_nodelay(true);
                let res = quic_tools::stream_client::probe(&mut sock, &hdr[..n], req)
                    .await
                    .expect("Plain TCP rtest.probe");
                let elapsed_us = t0.elapsed().as_micros() as u64;
                drop(sock);
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(res.bytes, sz);
                rows.push(PerfRow {
                    transport: "1. Plain TCP",
                    bearer: "Wi-Fi TCP",
                    operation: label,
                    bytes: sz,
                    stats: TransferStats {
                        elapsed_us,
                        tx_packets: 0,
                        rx_packets: 0,
                    },
                });
            }
        }

        let whdr = encode_flash_write_header(b"d_tcp", payload_1k.len(), 1090);
        let t0 = Instant::now();
        let mut wsock = TcpStream::connect(flash_addr)
            .await
            .expect("connect Plain TCP :5227");
        let _ = wsock.set_nodelay(true);
        let wresp = quic_tools::stream_client::exchange(&mut wsock, &[&whdr, &payload_1k])
            .await
            .expect("Plain TCP flash write");
        let w_us = t0.elapsed().as_micros() as u64;
        assert!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                .unwrap()
                .unwrap_or(0)
                >= 1
        );
        rows.push(PerfRow {
            transport: "1. Plain TCP",
            bearer: "Wi-Fi TCP",
            operation: "mod_flash write (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: w_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });

        let rhdr = encode_flash_read_header(b"d_tcp", 1091);
        let t1 = Instant::now();
        let mut rsock = TcpStream::connect(flash_addr)
            .await
            .expect("connect Plain TCP :5227");
        let _ = rsock.set_nodelay(true);
        let rresp = quic_tools::stream_client::exchange(&mut rsock, &[&rhdr])
            .await
            .expect("Plain TCP flash read");
        let r_us = t1.elapsed().as_micros() as u64;
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
        rows.push(PerfRow {
            transport: "1. Plain TCP",
            bearer: "Wi-Fi TCP",
            operation: "mod_flash read (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: r_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });
    }

    // ========================================================================
    // Transport 3: `quic-mux` only (`mod_mux` `52` over Wi-Fi TCP `:5229` & PPP `245`)
    // ========================================================================
    {
        sys_start(&serial_path, "mux", &[]).expect("start mux");
        sys_stop(&serial_path, "test").expect("stop test");
        sys_start(&serial_path, "test", b"probe").expect("start test in probe mode");

        // (a) Wi-Fi TCP `:5229` (`--stream 4`)
        let tcp_target = MuxClientTarget {
            tcp: Some(SocketAddr::new(device_ip, 5229)),
            ..Default::default()
        };
        let (cli_mux, _acc, _guard) = connect_mux_client(&tcp_target, TokioMuxConfig::default())
            .await
            .expect("connect mod_mux TCP :5229");

        let mut req_id = 3000u64;
        for (sz, up_label, down_label) in wifi_sizes {
            for (dir, label) in [
                (ProbeDirection::Upload, up_label),
                (ProbeDirection::Download, down_label),
            ] {
                req_id += 1;
                let mut req = ProbeServiceRequest::new(sz, 1024);
                req.direction = dir;
                req.verify = true;
                let t0 = Instant::now();
                let res = mux_probe(&cli_mux, req, req_id)
                    .await
                    .expect("mod_mux TCP probe");
                let elapsed_us = t0.elapsed().as_micros() as u64;
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(res.bytes, sz);
                rows.push(PerfRow {
                    transport: "3. mod_mux (quic-mux)",
                    bearer: "Wi-Fi TCP",
                    operation: label,
                    bytes: sz,
                    stats: TransferStats {
                        elapsed_us,
                        tx_packets: 0,
                        rx_packets: 0,
                    },
                });
            }
        }

        // Cross-transport check: read `d_tcp` written over Plain TCP, then write & read `d_mx_t`!
        let rprev = mux_exchange(&cli_mux, &encode_flash_read_header(b"d_tcp", 3089))
            .await
            .expect("mod_mux TCP read d_tcp");
        assert_eq!(split_flash_stream_response(&rprev).1, payload_1k.as_slice());

        let mut wreq = encode_flash_write_header(b"d_mx_t", payload_1k.len(), 3090);
        wreq.extend_from_slice(&payload_1k);
        let t0 = Instant::now();
        let wresp = mux_exchange(&cli_mux, &wreq)
            .await
            .expect("mod_mux TCP flash write");
        let w_us = t0.elapsed().as_micros() as u64;
        assert!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                .unwrap()
                .unwrap_or(0)
                >= 1
        );
        rows.push(PerfRow {
            transport: "3. mod_mux (quic-mux)",
            bearer: "Wi-Fi TCP",
            operation: "mod_flash write (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: w_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });

        let t1 = Instant::now();
        let rresp = mux_exchange(&cli_mux, &encode_flash_read_header(b"d_mx_t", 3091))
            .await
            .expect("mod_mux TCP flash read");
        let r_us = t1.elapsed().as_micros() as u64;
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
        rows.push(PerfRow {
            transport: "3. mod_mux (quic-mux)",
            bearer: "Wi-Fi TCP",
            operation: "mod_flash read (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: r_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });
        cli_mux.close();
        drop(cli_mux);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // (b) PPP serial (`--port 245`)
        let ser_target = MuxClientTarget {
            serial: Some(serial_path.clone()),
            ..Default::default()
        };
        let mut ser_cfg = TokioMuxConfig::default();
        ser_cfg.limits.initial_max_data = 16 * 1024;
        ser_cfg.limits.initial_max_stream_data = 8 * 1024;
        let (ser_mux, _ser_acc, mut ser_guard) = connect_mux_client(&ser_target, ser_cfg)
            .await
            .expect("connect mod_mux PPP 245");

        for (sz, up_label, down_label) in serial_sizes {
            for (dir, label) in [
                (ProbeDirection::Upload, up_label),
                (ProbeDirection::Download, down_label),
            ] {
                req_id += 1;
                let mut req = ProbeServiceRequest::new(sz, 1024);
                req.direction = dir;
                req.verify = true;
                let t0 = Instant::now();
                let res = mux_probe(&ser_mux, req, req_id)
                    .await
                    .expect("mod_mux PPP probe");
                let elapsed_us = t0.elapsed().as_micros() as u64;
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(res.bytes, sz);
                rows.push(PerfRow {
                    transport: "3. mod_mux (quic-mux)",
                    bearer: "PPP (245)",
                    operation: label,
                    bytes: sz,
                    stats: TransferStats {
                        elapsed_us,
                        tx_packets: 0,
                        rx_packets: 0,
                    },
                });
            }
        }

        let mut wreq_s = encode_flash_write_header(b"d_mx_s", payload_1k.len(), 3190);
        wreq_s.extend_from_slice(&payload_1k);
        let t0 = Instant::now();
        let wresp_s = mux_exchange(&ser_mux, &wreq_s)
            .await
            .expect("mod_mux PPP flash write");
        let w_us = t0.elapsed().as_micros() as u64;
        assert!(
            cbor::get_int(split_flash_stream_response(&wresp_s).0, 1)
                .unwrap()
                .unwrap_or(0)
                >= 1
        );
        rows.push(PerfRow {
            transport: "3. mod_mux (quic-mux)",
            bearer: "PPP (245)",
            operation: "mod_flash write (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: w_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });

        let t1 = Instant::now();
        let rresp_s = mux_exchange(&ser_mux, &encode_flash_read_header(b"d_mx_s", 3191))
            .await
            .expect("mod_mux PPP flash read");
        let r_us = t1.elapsed().as_micros() as u64;
        assert_eq!(split_flash_stream_response(&rresp_s).1, payload_1k.as_slice());
        rows.push(PerfRow {
            transport: "3. mod_mux (quic-mux)",
            bearer: "PPP (245)",
            operation: "mod_flash read (1 KiB)",
            bytes: 1024,
            stats: TransferStats {
                elapsed_us: r_us,
                tx_packets: 0,
                rx_packets: 0,
            },
        });

        ser_mux.close();
        drop(ser_mux);
        if let Some(mut g) = ser_guard.take() {
            g.shutdown().expect("shutdown serial mux guard");
        }

        let mux_status = serial_native_call(&serial_path, 52, 1, &[0xa0]).expect("mux.status");
        assert_eq!(cbor::get_int(&mux_status, 7).unwrap(), Some(0)); // 0 errors
        sys_stop(&serial_path, "mux").expect("stop mux");
    }

    // ========================================================================
    // Transport 4: `quic-link` standalone (`mod_qlink` `54` over Wi-Fi UDP `:3339` & PPP `242`)
    // ========================================================================
    {
        sys_stop(&serial_path, "test").expect("stop test");
        sys_start(&serial_path, "test", b"probe").expect("start test in probe mode");
        let _ = sys_stop(&serial_path, "quic");
        sys_start(&serial_path, "qlink", &[]).expect("start qlink");
        serial_native_call(&serial_path, 54, 2, &[0xa0]).expect("qlink.reset");
        let _ = serial_native_call(&serial_path, 54, 1, &[0xa0]).expect("qlink.status");
        let mut req_id = 4000u64;

        // (a) Wi-Fi UDP `:3339`
        {
            let mut cli = QlinkDeviceClient::new(
                PacketIo::new_udp(SocketAddr::new(device_ip, 3339)),
                1472,
                false,
            );
            for (sz, up_label, down_label) in wifi_sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, 1024).unwrap());
                    let mut reader = ProbeResponseReader::for_request(req);
                    let (_, stats) =
                        cli.execute_stream(&hdr[..n], upload, &[], Some(&mut reader));
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows.push(PerfRow {
                        transport: "4. mod_qlink (quic-link)",
                        bearer: "Wi-Fi UDP",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            // Cross-transport check: read `d_mx_s` written over `mod_mux` PPP!
            let (rprev, _) =
                cli.execute_stream(&encode_flash_read_header(b"d_mx_s", 4089), None, &[], None);
            assert_eq!(split_flash_stream_response(&rprev).1, payload_1k.as_slice());

            let whdr = encode_flash_write_header(b"d_qk_u", payload_1k.len(), 4090);
            let (wresp, wstats) = cli.execute_stream(&whdr, None, &payload_1k, None);
            assert!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                    .unwrap()
                    .unwrap_or(0)
                    >= 1
            );
            rows.push(PerfRow {
                transport: "4. mod_qlink (quic-link)",
                bearer: "Wi-Fi UDP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) =
                cli.execute_stream(&encode_flash_read_header(b"d_qk_u", 4091), None, &[], None);
            assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
            rows.push(PerfRow {
                transport: "4. mod_qlink (quic-link)",
                bearer: "Wi-Fi UDP",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });
        }

        // Reset `qlink` before switching to PPP serial so packet numbers start at 0 cleanly.
        let _ = sys_stop(&serial_path, "quic");
        serial_native_call(&serial_path, 54, 2, &[0xa0]).expect("qlink.reset");
        let _ = serial_native_call(&serial_path, 54, 1, &[0xa0]).expect("qlink.status");

        // (b) PPP serial (`--port 242`)
        {
            let mut cli = QlinkDeviceClient::new(
                PacketIo::new_serial(&serial_path, PORT_QLINK),
                PPP_MUX_MTU,
                true,
            );
            for (sz, up_label, down_label) in serial_sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, 1024).unwrap());
                    let mut reader = ProbeResponseReader::for_request(req);
                    let (_, stats) =
                        cli.execute_stream(&hdr[..n], upload, &[], Some(&mut reader));
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows.push(PerfRow {
                        transport: "4. mod_qlink (quic-link)",
                        bearer: "PPP (242)",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            let whdr = encode_flash_write_header(b"d_qk_s", payload_1k.len(), 4190);
            let (wresp, wstats) = cli.execute_stream(&whdr, None, &payload_1k, None);
            assert!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                    .unwrap()
                    .unwrap_or(0)
                    >= 1
            );
            rows.push(PerfRow {
                transport: "4. mod_qlink (quic-link)",
                bearer: "PPP (242)",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) =
                cli.execute_stream(&encode_flash_read_header(b"d_qk_s", 4191), None, &[], None);
            assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
            rows.push(PerfRow {
                transport: "4. mod_qlink (quic-link)",
                bearer: "PPP (242)",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });
        } // `cli` dropped here, releasing serial port before `serial_native_call`!

        let qlink_status = serial_native_call(&serial_path, 54, 1, &[0xa0]).expect("qlink.status");
        eprintln!(
            "qlink.status: rx={:?} tx={:?} retained={:?} errors={:?} last_error={:?}",
            cbor::get_int(&qlink_status, 2),
            cbor::get_int(&qlink_status, 3),
            cbor::get_int(&qlink_status, 4),
            cbor::get_int(&qlink_status, 5),
            cbor::get_int(&qlink_status, 6),
        );
        assert!(cbor::get_int(&qlink_status, 2).unwrap().unwrap_or(0) > 0);
        assert!(cbor::get_int(&qlink_status, 3).unwrap().unwrap_or(0) > 0);
        sys_stop(&serial_path, "qlink").expect("stop qlink");
    }

    // ========================================================================
    // Transport 5: `quic-link` + `quic-mux` (`mod_quic2` `55` over Wi-Fi UDP, PPP `243`, & Wi-Fi TCP `:5231`)
    // ========================================================================
    {
        sys_stop(&serial_path, "test").expect("stop test");
        sys_start(&serial_path, "test", b"probe").expect("start test in probe mode");
        let _ = sys_stop(&serial_path, "quic");
        sys_start(&serial_path, "quic2", &[]).expect("start quic2");
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        let _ = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");
        let mut req_id = 5000u64;

        // (a) Wi-Fi UDP `:3339`
        {
            let mut cli = Quic2DatagramDeviceClient::new(
                PacketIo::new_udp(SocketAddr::new(device_ip, 3339)),
                1472,
                false,
            );
            for (sz, up_label, down_label) in wifi_sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, 1024).unwrap());
                    let mut reader = ProbeResponseReader::for_request(req);
                    let (_, stats) =
                        cli.execute_stream(&hdr[..n], upload, &[], Some(&mut reader));
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows.push(PerfRow {
                        transport: "5. mod_quic2 (link+mux)",
                        bearer: "Wi-Fi UDP",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            // Cross-transport check: read `d_qk_s` written over `mod_qlink` PPP!
            let (rprev, _) =
                cli.execute_stream(&encode_flash_read_header(b"d_qk_s", 5089), None, &[], None);
            assert_eq!(split_flash_stream_response(&rprev).1, payload_1k.as_slice());

            let whdr = encode_flash_write_header(b"d_q2_u", payload_1k.len(), 5090);
            let (wresp, wstats) = cli.execute_stream(&whdr, None, &payload_1k, None);
            assert!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                    .unwrap()
                    .unwrap_or(0)
                    >= 1
            );
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "Wi-Fi UDP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) =
                cli.execute_stream(&encode_flash_read_header(b"d_q2_u", 5091), None, &[], None);
            assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "Wi-Fi UDP",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });
        }

        let _ = sys_stop(&serial_path, "quic");
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        let _ = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");

        // (b) PPP serial (`--port 243`)
        {
            let mut cli = Quic2DatagramDeviceClient::new(
                PacketIo::new_serial(&serial_path, PORT_QUIC2),
                PPP_MUX_MTU,
                true,
            );
            for (sz, up_label, down_label) in serial_sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, 1024).unwrap());
                    let mut reader = ProbeResponseReader::for_request(req);
                    let (_, stats) =
                        cli.execute_stream(&hdr[..n], upload, &[], Some(&mut reader));
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows.push(PerfRow {
                        transport: "5. mod_quic2 (link+mux)",
                        bearer: "PPP (243)",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            let whdr = encode_flash_write_header(b"d_q2_s", payload_1k.len(), 5190);
            let (wresp, wstats) = cli.execute_stream(&whdr, None, &payload_1k, None);
            assert!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                    .unwrap()
                    .unwrap_or(0)
                    >= 1
            );
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "PPP (243)",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) =
                cli.execute_stream(&encode_flash_read_header(b"d_q2_s", 5191), None, &[], None);
            assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "PPP (243)",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });
        } // `cli` dropped here, releasing serial port before `serial_native_call`!

        let _ = sys_stop(&serial_path, "quic");
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        let _ = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");

        // (c) Wi-Fi TCP `:5231` (`--stream 6`)
        {
            let mut tcp_cli =
                Quic2TcpDeviceClient::connect(SocketAddr::new(device_ip, 5231)).await;
            for (sz, up_label, down_label) in wifi_sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, 1024).unwrap());
                    let mut reader = ProbeResponseReader::for_request(req);
                    let (_, stats) = tcp_cli
                        .execute_stream(&hdr[..n], upload, &[], Some(&mut reader))
                        .await;
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows.push(PerfRow {
                        transport: "5. mod_quic2 (link+mux)",
                        bearer: "Wi-Fi TCP",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            let whdr = encode_flash_write_header(b"d_q2_t", payload_1k.len(), 5290);
            let (wresp, wstats) = tcp_cli
                .execute_stream(&whdr, None, &payload_1k, None)
                .await;
            assert!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1)
                    .unwrap()
                    .unwrap_or(0)
                    >= 1
            );
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "Wi-Fi TCP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) = tcp_cli
                .execute_stream(&encode_flash_read_header(b"d_q2_t", 5291), None, &[], None)
                .await;
            assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
            rows.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "Wi-Fi TCP",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });

            tcp_cli.close().await;
        }

        let quic2_status = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");
        eprintln!(
            "quic2.status: assocs={:?} streams={:?} tcp={:?} rx={:?} tx={:?} retained={:?} errors={:?} last_error={:?}",
            cbor::get_int(&quic2_status, 1),
            cbor::get_int(&quic2_status, 2),
            cbor::get_int(&quic2_status, 3),
            cbor::get_int(&quic2_status, 4),
            cbor::get_int(&quic2_status, 5),
            cbor::get_int(&quic2_status, 6),
            cbor::get_int(&quic2_status, 7),
            cbor::get_int(&quic2_status, 8),
        );
        assert!(cbor::get_int(&quic2_status, 4).unwrap().unwrap_or(0) > 0);
        assert!(cbor::get_int(&quic2_status, 5).unwrap().unwrap_or(0) > 0);
        sys_stop(&serial_path, "quic2").expect("stop quic2");
    }

    // Remove temporary test files from `mod_flash` (`files.remove` = component `16`, method `5`).
    for fname in [
        b"d_tcp".as_slice(),
        b"d_mx_t",
        b"d_mx_s",
        b"d_qk_u",
        b"d_qk_s",
        b"d_q2_u",
        b"d_q2_s",
        b"d_q2_t",
    ] {
        let mut fbuf = [0u8; 32];
        let mut fw = cbor::Writer::new(&mut fbuf);
        fw.map(1).uint(1).bytes(fname);
        let fields = fw.finish().unwrap();
        let _ = serial_native_call(&serial_path, 16, 5, fields);
    }

    println!("\n=== LIVE ESP32-C6 HARDWARE TRANSPORT PERFORMANCE BENCHMARKS ===");
    println!(
        "| {:<24} | {:<10} | {:<28} | {:>9} | {:>10} | {:>10} | {:>10} | {:>8} | {:>8} |",
        "Transport",
        "Bearer",
        "Operation",
        "Bytes",
        "Time (us)",
        "KiB/s",
        "Mbps",
        "TX Pkts",
        "RX Pkts"
    );
    println!(
        "|{:-<26}|{:-<12}|{:-<30}|{:->11}|{:->12}|{:->12}|{:->12}|{:->10}|{:->10}|",
        "", "", "", "", "", "", "", "", ""
    );
    for r in &rows {
        println!(
            "| {:<24} | {:<10} | {:<28} | {:>9} | {:>10} | {:>10.2} | {:>10.3} | {:>8} | {:>8} |",
            r.transport,
            r.bearer,
            r.operation,
            r.bytes,
            r.stats.elapsed_us,
            r.stats.kib_per_sec(r.bytes),
            r.stats.mbps(r.bytes),
            r.stats.tx_packets,
            r.stats.rx_packets
        );
    }
    println!("===============================================================\n");
}
