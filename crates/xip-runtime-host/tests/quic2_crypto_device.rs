//! Live ESP32 hardware benchmark for `mod_quic2` comparing Crypto OFF (`udp_encrypt = false`),
//! Crypto ON pre-keyed `ShortHeader`, and Crypto ON `Initial` + `ShortHeader` over Wi-Fi UDP `:3339`
//! plus PPP serial (`243`, `encrypt = false`) baseline.
//!
//! Opt-in via `XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...` (and optional `XIP_TRANSPORT_IP`).
//! Never hardcodes any board name, serial path, MAC, or IP address.

use dmesh_cli::{
    mux_cli::{ProbeDirection, ProbeServiceRequest},
    xip_wire::{Event, Reader, XipPort, encode_record, unwrap_answer, wrap_request},
};
use quic_base::{
    AEAD_TAG_LEN, CryptoError, CryptoProvider, CryptoRole, PUBLIC_KEY_COMPRESSED_LEN,
    TRAFFIC_IV_LEN, TRAFFIC_KEY_LEN, TransportParameters, derive_device_secret_with,
    derive_peer_api_key_with,
};
use quic_link::{
    AssocConfig, AssocCryptoConfig, CongestionMode, ConnectionId, HandshakeMode, LinkFraming,
    LinkId, LinkProfile, LinkTxOutcome, QuicLinkAssoc,
    flow::StreamPriority,
    link::OWNER_ASSOC,
    pool::{PacketPool as LinkPacketPool, PoolPacket as LinkPoolPacket, StaticPacketLease, StaticPacketPool},
};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, QuicMux, RxMeta,
    RxOutcome as MuxRxOutcome, RxStreamChunk as MuxRxStreamChunk, StreamCloseReason, StreamError,
    StreamId, TxStreamData as MuxTxStreamData,
};
use quic_tools::{ProbePayload, ProbeResponseReader, encode_probe_handler_request};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};
use xip_abi::cbor;

const PORT_QUIC: u32 = 247;
const PORT_QUIC2: u32 = 243;

fn psa_sha256(_ctx: usize, chunks: &[&[u8]], out: &mut [u8; 32]) -> Result<(), CryptoError> {
    let mut h = xip_psa::Hasher::new(xip_psa::ALG_SHA_256).ok_or(CryptoError::Invalid)?;
    for c in chunks {
        h.update(c);
    }
    out.copy_from_slice(&h.finish());
    Ok(())
}

fn psa_seal_gcm(
    _ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
) -> Result<[u8; AEAD_TAG_LEN], CryptoError> {
    xip_psa::aead_seal_in_place(xip_psa::ALG_GCM, key, nonce, aad, body)
        .ok_or(CryptoError::Invalid)
}

fn psa_open_gcm(
    _ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; AEAD_TAG_LEN],
) -> Result<(), CryptoError> {
    if xip_psa::aead_open_in_place(xip_psa::ALG_GCM, key, nonce, aad, body, tag) {
        Ok(())
    } else {
        Err(CryptoError::AuthenticationFailed)
    }
}

fn psa_seal_ccm(
    _ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
) -> Result<[u8; AEAD_TAG_LEN], CryptoError> {
    xip_psa::aead_seal_in_place(xip_psa::ALG_CCM, key, nonce, aad, body)
        .ok_or(CryptoError::Invalid)
}

fn psa_open_ccm(
    _ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; AEAD_TAG_LEN],
) -> Result<(), CryptoError> {
    if xip_psa::aead_open_in_place(xip_psa::ALG_CCM, key, nonce, aad, body, tag) {
        Ok(())
    } else {
        Err(CryptoError::AuthenticationFailed)
    }
}

fn host_psa_provider() -> CryptoProvider {
    host_psa_provider_cipher(0)
}

fn host_psa_provider_cipher(cipher: u8) -> CryptoProvider {
    if cipher == 1 {
        CryptoProvider {
            context: 0,
            sha256: psa_sha256,
            aead_seal: psa_seal_ccm,
            aead_open: psa_open_ccm,
            aead_packet: None,
        }
    } else {
        CryptoProvider {
            context: 0,
            sha256: psa_sha256,
            aead_seal: psa_seal_gcm,
            aead_open: psa_open_gcm,
            aead_packet: None,
        }
    }
}

fn serial_native_call(
    serial_path: &str,
    component: u64,
    method: u64,
    fields_cbor: &[u8],
) -> Result<Vec<u8>, String> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(920_000);
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
        Err(e) if e.contains("error -103") => {}
        Err(e) => return Err(e),
    }
    wait_module_running(serial_path, name.as_bytes(), true)?;
    if name == "test" && request == b"probe" {
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
        Err(e) if e.contains("error -7") => Ok(()),
        Err(e) => Err(e),
    }
}

fn configure_quic2_device(
    serial_path: &str,
    udp_encrypt: bool,
    serial_encrypt: bool,
    cipher: u8,
    node_secret: Option<&[u8; 32]>,
) -> Result<(), String> {
    let mut buf = [0u8; 96];
    let mut w = cbor::Writer::new(&mut buf);
    w.map(6)
        .uint(1)
        .boolean(udp_encrypt)
        .uint(2)
        .bytes(node_secret.map(|s| s.as_slice()).unwrap_or(&[]))
        .uint(3)
        .bytes(&[])
        .uint(4)
        .boolean(serial_encrypt)
        .uint(5)
        .boolean(udp_encrypt)
        .uint(6)
        .uint(cipher as u64);
    let resp = serial_native_call(serial_path, 55, 3, w.finish().unwrap())?;
    if cbor::get(&resp, 1).ok().flatten().and_then(|mut r| r.next())
        != Some(cbor::Item::Bool(udp_encrypt))
        || cbor::get(&resp, 3).ok().flatten().and_then(|mut r| r.next())
            != Some(cbor::Item::Bool(serial_encrypt))
        || cbor::get_int(&resp, 5).ok().flatten() != Some(cipher as i64)
    {
        return Err(format!("quic2.config encrypt/cipher mismatch: {resp:02x?}"));
    }
    Ok(())
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
        std::thread::sleep(Duration::from_millis(400));
    }
    Err("device STA has no IPv4 address in sys.status; set XIP_TRANSPORT_IP or wake NAN".into())
}

enum PacketIo {
    Udp(UdpSocket),
    Tcp {
        stream: std::net::TcpStream,
        rx_buf: Vec<u8>,
    },
    Serial {
        port: XipPort,
        ppp_port: u32,
        reader: Reader,
    },
}

impl PacketIo {
    fn new_udp(peer: SocketAddr) -> Self {
        let bind_addr: SocketAddr = match peer {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let sock = UdpSocket::bind(bind_addr).expect("bind UDP client");
        sock.connect(peer).expect("connect UDP client");
        sock.set_read_timeout(Some(Duration::from_millis(2)))
            .expect("set_read_timeout UDP");
        sock.set_nonblocking(true).expect("set_nonblocking UDP");
        Self::Udp(sock)
    }

    fn new_tcp(peer: SocketAddr) -> Self {
        let stream = std::net::TcpStream::connect_timeout(&peer, Duration::from_secs(5))
            .expect("connect TCP client");
        stream.set_nodelay(true).expect("set_nodelay TCP");
        stream
            .set_read_timeout(Some(Duration::from_millis(2)))
            .expect("set_read_timeout TCP");
        stream.set_nonblocking(true).expect("set_nonblocking TCP");
        Self::Tcp {
            stream,
            rx_buf: Vec::with_capacity(4096),
        }
    }

    fn new_serial(serial_path: &str, ppp_port: u32, baud: Option<u32>) -> Self {
        let port = XipPort::open_baud(serial_path.to_owned(), baud).expect("open serial port");
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
            Self::Tcp { stream, .. } => {
                use std::io::Write;
                let hdr = quic_link::encode_tls_record_header(bytes.len()).expect("tls record len");
                let mut frame = Vec::with_capacity(hdr.len() + bytes.len());
                frame.extend_from_slice(&hdr);
                frame.extend_from_slice(bytes);
                let _ = stream.set_nonblocking(false);
                stream.write_all(&frame).expect("write TCP TLS record");
                let _ = stream.set_nonblocking(true);
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
            Self::Tcp { stream, rx_buf } => {
                use std::io::Read;
                let mut tmp = [0u8; 4096];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => rx_buf.extend_from_slice(&tmp[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                loop {
                    match quic_link::decode_tls_record_header(rx_buf, 1500) {
                        Ok(Some(payload_len)) => {
                            let total = quic_link::TLS_RECORD_HEADER_LEN + payload_len;
                            if rx_buf.len() < total {
                                break;
                            }
                            out.push(
                                rx_buf[quic_link::TLS_RECORD_HEADER_LEN..total].to_vec(),
                            );
                            rx_buf.drain(..total);
                        }
                        Ok(None) => break,
                        Err(()) => panic!("invalid TLS record header on TCP"),
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

    fn wait_readable(&self) {
        match self {
            Self::Udp(sock) => {
                let _ = sock.set_nonblocking(false);
                let mut b = [0u8; 1];
                let _ = sock.peek(&mut b);
                let _ = sock.set_nonblocking(true);
            }
            Self::Tcp { stream, .. } => {
                let _ = stream.set_nonblocking(false);
                let mut b = [0u8; 1];
                let _ = stream.peek(&mut b);
                let _ = stream.set_nonblocking(true);
            }
            Self::Serial { .. } => {
                std::thread::sleep(Duration::from_micros(250));
            }
        }
    }
}

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
    fn on_stream_closed(&mut self, _: AssociationId, sid: StreamId, reason: StreamCloseReason) {
        eprintln!("  [client] on_stream_closed sid={sid:?} reason={reason:?}");
    }
    fn on_association_broken(&mut self, _: AssociationId, reason: AssociationBrokenReason) {
        eprintln!("  [client] on_association_broken reason={reason:?}");
    }
}

struct DeviceStats {
    elapsed_us: u64,
    tx_packets: u64,
    tx_wire_bytes: u64,
    rx_packets: u64,
    rx_wire_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceCryptoMode {
    Off,
    PrekeyedGcm,
    InitialGcm,
    PrekeyedCcm,
    InitialCcm,
}

impl DeviceCryptoMode {
    fn is_initial(self) -> bool {
        matches!(self, Self::InitialGcm | Self::InitialCcm)
    }
    fn cipher(self) -> u8 {
        match self {
            Self::PrekeyedCcm | Self::InitialCcm => 1,
            _ => 0,
        }
    }
}

struct Quic2CryptoClient {
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    io: PacketIo,
    start_instant: Instant,
}

impl Quic2CryptoClient {
    fn new(
        io: PacketIo,
        mtu: usize,
        is_serial: bool,
        mode: DeviceCryptoMode,
        node_secret: Option<[u8; 32]>,
    ) -> Self {
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
        let client_pub = [0x02u8; PUBLIC_KEY_COMPRESSED_LEN];
        let mut assoc = QuicLinkAssoc::new(AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: if mode.is_initial() {
                ConnectionId::new(22)
            } else {
                None
            },
            default_peer_cid: cid,
            max_retained_tx_packets: if is_serial { 6 } else { 8 },
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 15_000,
            handshake_mode: if mode.is_initial() {
                HandshakeMode::Client
            } else {
                HandshakeMode::None
            },
            local_params: if mode.is_initial() {
                TransportParameters {
                    initial_scid: ConnectionId::new(22),
                    initial_max_data: Some(64 * 1024),
                    initial_max_stream_data_bidi_local: Some(32 * 1024),
                    initial_max_stream_data_bidi_remote: Some(32 * 1024),
                    initial_max_streams_bidi: Some(8),
                    initial_max_streams_uni: Some(8),
                    public_key: Some(client_pub),
                    ..Default::default()
                }
            } else {
                TransportParameters::default()
            },
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
                framing: if matches!(io, PacketIo::Tcp { .. }) {
                    LinkFraming::TlsRecord { peer_cid: cid }
                } else {
                    LinkFraming::Datagram { peer_cid: cid }
                },
                initial_rtt_us: if is_serial { 120_000 } else { 20_000 },
                encrypt: mode != DeviceCryptoMode::Off,
            },
        );
        if mode != DeviceCryptoMode::Off {
            let prov = host_psa_provider_cipher(mode.cipher());
            let ns = node_secret.expect("node_secret required when crypto=ON");
            let api_key = if mode.is_initial() {
                derive_peer_api_key_with(&prov, &ns, &client_pub).unwrap()
            } else {
                derive_peer_api_key_with(&prov, &ns, &[]).unwrap()
            };
            assoc
                .set_crypto_config(AssocCryptoConfig {
                    provider: prov,
                    role: CryptoRole::Client,
                    local_public_key: mode.is_initial().then_some(client_pub),
                    local_random: [0x22; 32],
                    send_api_key: Some(api_key),
                    node_secret: None,
                })
                .unwrap();
        }
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

    fn execute_probe(
        &mut self,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        mut probe_reader: ProbeResponseReader,
        with_initial: bool,
    ) -> DeviceStats {
        let pool_ref = self.pool_ref;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(120);
        let mut last_rx_progress = started;
        let mut last_rx_count = 0u64;

        if with_initial {
            let mut pr = pool_ref;
            self.assoc.enqueue_initial(&mut pr).unwrap();
        }

        let stream_id = self.mux.open_stream(OWNER_ASSOC, true).unwrap();
        let mut tx_packets = 0u64;
        let mut tx_wire_bytes = 0u64;
        let mut rx_packets = 0u64;
        let mut rx_wire_bytes = 0u64;

        let mut hdr_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1024];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;
        let mut rx_cb = Quic2ClientRx::default();

        while Instant::now() < deadline {
            let now_us = self.now_us();

            if !with_initial || self.assoc.is_handshake_complete() {
                while !tx_fin_sent {
                    if !self.assoc.can_admit_priority(StreamPriority::NORMAL) {
                        break;
                    }
                    let Ok(cap) = self.mux.stream_send_capacity(OWNER_ASSOC, stream_id, &self.assoc)
                    else {
                        break;
                    };
                    if !cap.can_send_now || cap.max_chunk_bytes == 0 {
                        break;
                    }
                    let max_w = cap.max_chunk_bytes;
                    if hdr_at < header.len() {
                        let has_more = upload_payload.is_some();
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
            }

            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);

            let mut moved = false;
            while let Some(out) = self.assoc.pop_outbound_packet() {
                tx_wire_bytes += out.packet.bytes().len() as u64;
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
                    rx_wire_bytes += wire.len() as u64;
                    let Ok(mut p) = pool_ref.alloc() else {
                        break;
                    };
                    p.append(&wire).unwrap();
                    match self.assoc.receive_packet(self.link_id, p, now_rx) {
                        Ok(demux) => {
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
                        Err(e) => {
                            eprintln!("  [client rx err] wire_len={} err={e:?}", wire.len());
                        }
                    }
                }
                if let Some((buf, _)) = rx_cb.streams.get_mut(&stream_id) {
                    if !buf.is_empty() {
                        probe_reader.push(buf).expect("valid probe response chunk");
                        buf.clear();
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
                tx_wire_bytes += out.packet.bytes().len() as u64;
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
            if rx_packets != last_rx_count {
                last_rx_count = rx_packets;
                last_rx_progress = Instant::now();
            } else if last_rx_progress.elapsed() > Duration::from_secs(10) {
                break;
            }
            if !moved {
                self.io.wait_readable();
            }
        }
        let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
        assert!(
            tx_fin_sent && rx_fin,
            "Quic2CryptoClient timed out (hs_done={}, tx_fin={tx_fin_sent}, rx_fin={rx_fin}, tx={tx_packets}, rx={rx_packets}, rx_bytes={rx_wire_bytes}, host_scap={:?}, pending_retx={}, out_q={}, next_dl={:?}, ack_ranges={:?})",
            self.assoc.is_handshake_complete(),
            self.mux.stream_send_capacity(OWNER_ASSOC, stream_id, &self.assoc),
            self.assoc.pending_retransmits_len(),
            self.assoc.outbound_queue_len(),
            self.assoc.next_deadline_us(),
            self.assoc.rx_ack_ranges(),
        );

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        let ack_ts = self.now_us() + 50_000;
        let mut pr = pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, ack_ts);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            tx_wire_bytes += ack.packet.bytes().len() as u64;
            self.io.send_packet(ack.packet.bytes());
            self.assoc.on_driver_complete(
                self.link_id,
                ack.packet_number,
                LinkTxOutcome::Sent,
                ack_ts,
            );
            tx_packets += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
        for wire in self.io.recv_packets() {
            rx_packets += 1;
            rx_wire_bytes += wire.len() as u64;
        }

        if let Some((rem, _)) = rx_cb.streams.remove(&stream_id) {
            if !rem.is_empty() {
                probe_reader.push(&rem).expect("valid final probe response");
            }
        }
        assert!(probe_reader.is_complete(), "probe incomplete at stream FIN");

        DeviceStats {
            elapsed_us: started.elapsed().as_micros() as u64,
            tx_packets,
            tx_wire_bytes,
            rx_packets,
            rx_wire_bytes,
        }
    }
}

struct DeviceCleanupGuard {
    serial_path: String,
}

impl Drop for DeviceCleanupGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Ok(st) = serial_native_call(&self.serial_path, 55, 1, &[0xa0]) {
                let rx = cbor::get_int(&st, 4).ok().flatten();
                let tx = cbor::get_int(&st, 5).ok().flatten();
                let err = cbor::get_int(&st, 7).ok().flatten();
                let lerr = cbor::get_int(&st, 8).ok().flatten();
                let scap = cbor::get_int(&st, 10).ok().flatten();
                let inflight = cbor::get_int(&st, 11).ok().flatten();
                eprintln!("  [panic quic2.status] rx={rx:?} tx={tx:?} err={err:?} last_err={lerr:?} scap={scap:?} inflight={inflight:?}");
            }
        }
        let _ = configure_quic2_device(&self.serial_path, true, false, 0, None);
        let _ = serial_native_call(&self.serial_path, 55, 2, &[0xa0]);
        let _ = sys_stop(&self.serial_path, "quic2");
        let _ = sys_stop(&self.serial_path, "test");
        let _ = sys_start(&self.serial_path, "quic", &[]);
        let mut sw = [0u8; 32];
        let mut cw = cbor::Writer::new(&mut sw);
        cw.map(1).uint(1).boolean(true);
        let _ = serial_native_call(&self.serial_path, 46, 6, cw.finish().unwrap());
    }
}

#[test]
#[ignore]
fn live_esp32_mod_quic2_crypto_off_vs_on_benchmarks() {
    let Ok(serial_path) = std::env::var("XIP_TRANSPORT_DEVICE") else {
        eprintln!("skipping: set XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...");
        return;
    };
    let _guard = DeviceCleanupGuard {
        serial_path: serial_path.clone(),
    };

    // Keep NAN awake and STA online throughout benchmark.
    let _ = sys_start(&serial_path, "nan", &[]);
    let mut sleepy_buf = [0u8; 32];
    let mut sw = cbor::Writer::new(&mut sleepy_buf);
    sw.map(2).uint(1).boolean(false).uint(4).boolean(true);
    let _ = serial_native_call(&serial_path, 46, 6, sw.finish().unwrap());
    let mut awake_buf = [0u8; 32];
    let mut aw = cbor::Writer::new(&mut awake_buf);
    aw.map(1).uint(1).uint(300_000);
    let _ = serial_native_call(&serial_path, 46, 10, aw.finish().unwrap());

    sys_stop(&serial_path, "mux").expect("stop mux");
    sys_stop(&serial_path, "qlink").expect("stop qlink");
    let _ = sys_stop(&serial_path, "quic");
    sys_stop(&serial_path, "test").expect("stop test");
    sys_start(&serial_path, "test", b"probe").expect("start test probe");
    sys_start(&serial_path, "quic2", &[]).expect("start quic2");

    let device_ip = discover_device_ip(&serial_path).expect("discover device IP");
    let prov = host_psa_provider();
    let node_secret = derive_device_secret_with(
        &prov,
        b"live-esp32-crypto-bench-secret-material",
        &[0x03u8; PUBLIC_KEY_COMPRESSED_LEN],
    )
    .unwrap();

    println!("=== Live ESP32 mod_quic2 Crypto OFF vs ON on {serial_path} (IP {device_ip}) ===");
    println!(
        "{:<10} {:<20} {:<12} {:>10} {:>8} {:>8} {:>11} {:>11}",
        "Bearer", "Mode", "Operation", "Elapsed us", "KiB/s", "Mbps", "TX pkt/B", "RX pkt/B"
    );

    let mut req_id = 7000u64;

    // 1. PPP Serial (port 243, unified MTU 1472): Crypto OFF vs Prekeyed GCM & CCM
    let ppp_sizes: &[u64] = &[64, 1024, 1400, 16 * 1024];
    for &(mode, label, serial_encrypt, set_secret) in &[
        (DeviceCryptoMode::Off, "crypto=OFF", false, false),
        (DeviceCryptoMode::PrekeyedGcm, "GCM(prekey)", true, true),
        (DeviceCryptoMode::PrekeyedCcm, "CCM(prekey)", true, true),
    ] {
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        configure_quic2_device(
            &serial_path,
            false,
            serial_encrypt,
            mode.cipher(),
            set_secret.then_some(&node_secret),
        )
        .expect("config PPP");
        {
            let mut cli = Quic2CryptoClient::new(
                PacketIo::new_serial(&serial_path, PORT_QUIC2, None),
                1472,
                true,
                mode,
                set_secret.then_some(node_secret),
            );
            for &sz in ppp_sizes {
                for dir in [ProbeDirection::Upload, ProbeDirection::Download] {
                    req_id += 1;
                    let pkt_sz = (sz as usize).min(1024) as u16;
                    let mut req = ProbeServiceRequest::new(sz, pkt_sz);
                    req.direction = dir;
                    req.verify = true;
                    let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                    let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                    let upload = (dir == ProbeDirection::Upload)
                        .then(|| ProbePayload::new(sz, pkt_sz as usize).unwrap());
                    let reader = ProbeResponseReader::for_request(req);
                    let st = cli.execute_probe(&hdr[..n], upload, reader, false);
                    let op = if dir == ProbeDirection::Upload {
                        format!("up({sz})")
                    } else {
                        format!("dn({sz})")
                    };
                    println!(
                        "{:<10} {:<20} {:<12} {:>10} {:>8.2} {:>8.3} {:>4}/{:<6} {:>4}/{:<6}",
                        "PPP(243)",
                        label,
                        op,
                        st.elapsed_us,
                        (sz as f64 / 1024.0) / (st.elapsed_us as f64 / 1e6),
                        (sz as f64 * 8.0) / (st.elapsed_us as f64),
                        st.tx_packets,
                        st.tx_wire_bytes,
                        st.rx_packets,
                        st.rx_wire_bytes
                    );
                }
            }
        }
        let status = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");
        let errs = cbor::get_int(&status, 7).ok().flatten().unwrap_or(-1);
        let last_err = cbor::get_int(&status, 8).ok().flatten().unwrap_or(0);
        println!("  [quic2.status PPP {label}] errors={errs} last_error={last_err}");
        assert_eq!(errs, 0, "unexpected mod_quic2 PPP errors for {label}");
    }

    // 2. Wi-Fi UDP :3339 Crypto OFF vs Prekeyed/Initial GCM vs Prekeyed/Initial CCM
    let udp_sizes: &[u64] = &[64, 256, 1024, 1200, 1400, 64 * 1024, 256 * 1024, 1024 * 1024];
    let init_sizes: &[u64] = &[64, 1024, 64 * 1024];

    for &(mode, label, encrypt, set_secret, sizes) in &[
        (DeviceCryptoMode::Off, "crypto=OFF", false, false, udp_sizes),
        (DeviceCryptoMode::PrekeyedGcm, "GCM(prekey)", true, true, udp_sizes),
        (DeviceCryptoMode::PrekeyedCcm, "CCM(prekey)", true, true, udp_sizes),
        (DeviceCryptoMode::InitialGcm, "GCM(initial)", true, true, init_sizes),
        (DeviceCryptoMode::InitialCcm, "CCM(initial)", true, true, init_sizes),
    ] {
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        configure_quic2_device(
            &serial_path,
            encrypt,
            false,
            mode.cipher(),
            set_secret.then_some(&node_secret),
        )
        .expect("configure quic2");
        let mut reusable_cli = (!mode.is_initial()).then(|| {
            Quic2CryptoClient::new(
                PacketIo::new_udp(SocketAddr::new(device_ip, 3339)),
                1472,
                false,
                mode,
                set_secret.then_some(node_secret),
            )
        });

        for &sz in sizes {
            for dir in [ProbeDirection::Upload, ProbeDirection::Download] {
                let mut per_conn_cli;
                let cli = if let Some(c) = reusable_cli.as_mut() {
                    c
                } else {
                    serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
                    per_conn_cli = Quic2CryptoClient::new(
                        PacketIo::new_udp(SocketAddr::new(device_ip, 3339)),
                        1472,
                        false,
                        mode,
                        Some(node_secret),
                    );
                    &mut per_conn_cli
                };

                req_id += 1;
                let pkt_sz = (sz as usize).min(1024) as u16;
                let mut req = ProbeServiceRequest::new(sz, pkt_sz);
                req.direction = dir;
                req.verify = true;
                let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                let upload = (dir == ProbeDirection::Upload)
                    .then(|| ProbePayload::new(sz, pkt_sz as usize).unwrap());
                let reader = ProbeResponseReader::for_request(req);
                let st = cli.execute_probe(
                    &hdr[..n],
                    upload,
                    reader,
                    mode.is_initial(),
                );
                let op = if dir == ProbeDirection::Upload {
                    format!("up({sz})")
                } else {
                    format!("dn({sz})")
                };
                println!(
                    "{:<10} {:<20} {:<12} {:>10} {:>8.2} {:>8.3} {:>4}/{:<6} {:>4}/{:<6}",
                    "UDP(:3339)",
                    label,
                    op,
                    st.elapsed_us,
                    (sz as f64 / 1024.0) / (st.elapsed_us as f64 / 1e6),
                    (sz as f64 * 8.0) / (st.elapsed_us as f64),
                    st.tx_packets,
                    st.tx_wire_bytes,
                    st.rx_packets,
                    st.rx_wire_bytes
                );
            }
        }

        let status = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");
        let errs = cbor::get_int(&status, 7).ok().flatten().unwrap_or(-1);
        let last_err = cbor::get_int(&status, 8).ok().flatten().unwrap_or(0);
        println!("  [quic2.status {label}] errors={errs} last_error={last_err}");
        assert_eq!(errs, 0, "unexpected mod_quic2 errors for {label}");
    }
}

#[test]
#[ignore]
fn live_esp32_mod_quic2_10m_benchmark() {
    let Ok(serial_path) = std::env::var("XIP_TRANSPORT_DEVICE") else {
        eprintln!("skipping: set XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...");
        return;
    };
    let _guard = DeviceCleanupGuard {
        serial_path: serial_path.clone(),
    };

    let _ = sys_start(&serial_path, "nan", &[]);
    let mut sleepy_buf = [0u8; 32];
    let mut sw = cbor::Writer::new(&mut sleepy_buf);
    sw.map(2).uint(1).boolean(false).uint(4).boolean(true);
    let _ = serial_native_call(&serial_path, 46, 6, sw.finish().unwrap());
    let mut awake_buf = [0u8; 32];
    let mut aw = cbor::Writer::new(&mut awake_buf);
    aw.map(1).uint(1).uint(600_000);
    let _ = serial_native_call(&serial_path, 46, 10, aw.finish().unwrap());

    sys_stop(&serial_path, "mux").expect("stop mux");
    sys_stop(&serial_path, "qlink").expect("stop qlink");
    let _ = sys_stop(&serial_path, "quic");
    sys_stop(&serial_path, "test").expect("stop test");
    sys_start(&serial_path, "test", b"probe").expect("start test probe");
    sys_start(&serial_path, "quic2", &[]).expect("start quic2");

    let (use_tcp, peer_addr): (bool, SocketAddr) = if let Ok(raw) = std::env::var("XIP_TRANSPORT_PEER") {
        let (is_tcp, addr_str) = if let Some(rest) = raw.strip_prefix("tcp://") {
            (true, rest)
        } else {
            (false, raw.as_str())
        };
        // Supports IPv4 "1.2.3.4:3339" or IPv6 link-local "[fe80::...%3]:3339".
        let sa = if let Some(rest) = addr_str.strip_prefix('[') {
            let (host_scope, port_s) = rest.split_once("]:").expect("valid [ipv6]:port");
            let port: u16 = port_s.parse().expect("valid port");
            let (ip_s, scope_id) = match host_scope.split_once('%') {
                Some((ip, sc)) => (ip, sc.parse::<u32>().expect("numeric scope_id")),
                None => (host_scope, 0),
            };
            let ip6: Ipv6Addr = ip_s.parse().expect("valid ipv6");
            SocketAddr::V6(std::net::SocketAddrV6::new(ip6, port, 0, scope_id))
        } else {
            addr_str.parse().expect("valid SocketAddr")
        };
        (is_tcp, sa)
    } else {
        let device_ip = discover_device_ip(&serial_path).expect("discover device IP");
        (false, SocketAddr::new(device_ip, 3339))
    };
    let udp_mtu = if peer_addr.is_ipv6() { 1452 } else { 1472 };
    let bearer_label = if use_tcp {
        if peer_addr.is_ipv6() { "TCP6(:5231)" } else { "TCP(:5231)" }
    } else if peer_addr.is_ipv6() {
        "UDP6(:3339)"
    } else {
        "UDP(:3339)"
    };
    let prov = host_psa_provider();
    let node_secret = derive_device_secret_with(
        &prov,
        b"live-esp32-crypto-bench-secret-material",
        &[0x03u8; PUBLIC_KEY_COMPRESSED_LEN],
    )
    .unwrap();

    let sz: u64 = std::env::var("XIP_TRANSPORT_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10 * 1024 * 1024);
    println!("=== Live ESP32 mod_quic2 Benchmark ({sz} B) on {serial_path} (peer {peer_addr}) ===");
    println!(
        "{:<11} {:<20} {:<12} {:>10} {:>8} {:>8} {:>11} {:>11}",
        "Bearer", "Mode", "Operation", "Elapsed us", "KiB/s", "Mbps", "TX pkt/B", "RX pkt/B"
    );

    let mut req_id = 9000u64;
    for &(mode, label, encrypt, set_secret) in &[
        (DeviceCryptoMode::Off, "crypto=OFF", false, false),
        (DeviceCryptoMode::PrekeyedGcm, "GCM(prekey)", true, true),
        (DeviceCryptoMode::PrekeyedCcm, "CCM(prekey)", true, true),
    ] {
        serial_native_call(&serial_path, 55, 2, &[0xa0]).expect("quic2.reset");
        configure_quic2_device(
            &serial_path,
            encrypt,
            encrypt,
            mode.cipher(),
            set_secret.then_some(&node_secret),
        )
        .expect("configure quic2");
        let io = if use_tcp {
            PacketIo::new_tcp(peer_addr)
        } else {
            PacketIo::new_udp(peer_addr)
        };
        let mut cli = Quic2CryptoClient::new(
            io,
            udp_mtu,
            false,
            mode,
            set_secret.then_some(node_secret),
        );
        for dir in [ProbeDirection::Upload, ProbeDirection::Download] {
            req_id += 1;
            let mut req = ProbeServiceRequest::new(sz, 1024);
            req.direction = dir;
            req.verify = true;
            let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
            let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
            let upload = (dir == ProbeDirection::Upload)
                .then(|| ProbePayload::new(sz, 1024).unwrap());
            let reader = ProbeResponseReader::for_request(req);
            let st = cli.execute_probe(&hdr[..n], upload, reader, false);
            let op = if dir == ProbeDirection::Upload {
                format!("up({sz})")
            } else {
                format!("dn({sz})")
            };
            println!(
                "{:<11} {:<20} {:<12} {:>10} {:>8.2} {:>8.3} {:>4}/{:<6} {:>4}/{:<6}",
                bearer_label,
                label,
                op,
                st.elapsed_us,
                (sz as f64 / 1024.0) / (st.elapsed_us as f64 / 1e6),
                (sz as f64 * 8.0) / (st.elapsed_us as f64),
                st.tx_packets,
                st.tx_wire_bytes,
                st.rx_packets,
                st.rx_wire_bytes
            );
        }
        let status = serial_native_call(&serial_path, 55, 1, &[0xa0]).expect("quic2.status");
        let errs = cbor::get_int(&status, 7).ok().flatten().unwrap_or(-1);
        let last_err = cbor::get_int(&status, 8).ok().flatten().unwrap_or(0);
        println!("  [quic2.status {label}] errors={errs} last_error={last_err}");
        assert_eq!(errs, 0, "unexpected mod_quic2 errors for {label}");
    }
}

#[test]
#[ignore = "requires connected ESP32 hardware via XIP_TRANSPORT_DEVICE"]
fn live_esp32_ppp_high_baud_crypto_bench() {
    let Ok(serial_path) = std::env::var("XIP_TRANSPORT_DEVICE") else {
        eprintln!("skipping: set XIP_TRANSPORT_DEVICE=/dev/serial/by-id/...");
        return;
    };
    let bauds: Vec<u32> = if let Ok(raw) = std::env::var("XIP_TRANSPORT_BAUDS") {
        raw.split(',')
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .collect()
    } else {
        vec![921_600, 1_000_000, 2_000_000]
    };

    let _ = sys_start(&serial_path, "nan", &[]);
    let mut sbuf = [0u8; 32];
    let mut sw = cbor::Writer::new(&mut sbuf);
    sw.map(1).uint(1).boolean(false);
    let _ = serial_native_call(&serial_path, 46, 6, sw.finish().unwrap());
    let mut abuf = [0u8; 32];
    let mut aw = cbor::Writer::new(&mut abuf);
    aw.map(1).uint(1).uint(300_000);
    let _ = serial_native_call(&serial_path, 46, 10, aw.finish().unwrap());

    sys_stop(&serial_path, "mux").expect("stop mux");
    sys_stop(&serial_path, "qlink").expect("stop qlink");
    let _ = sys_stop(&serial_path, "quic");
    sys_stop(&serial_path, "test").expect("stop test");
    sys_start(&serial_path, "test", b"probe").expect("start test probe");
    sys_start(&serial_path, "quic2", &[]).expect("start quic2");

    let prov = host_psa_provider();
    let node_secret = derive_device_secret_with(
        &prov,
        b"live-esp32-crypto-bench-secret-material",
        &[0x03u8; PUBLIC_KEY_COMPRESSED_LEN],
    )
    .unwrap();

    struct RestoreBaud(String, u32);
    impl Drop for RestoreBaud {
        fn drop(&mut self) {
            if self.1 != 115_200 {
                if let Ok(mut p) = XipPort::open_baud(self.0.clone(), Some(self.1)) {
                    let _ = p.negotiate_baud(115_200);
                }
            }
        }
    }

    println!("=== Live ESP32 mod_quic2 High-Baud PPP Crypto OFF vs ON on {serial_path} ===");
    println!(
        "{:<12} {:<14} {:<12} {:>10} {:>8} {:>8} {:>11} {:>11}",
        "Baud", "Mode", "Operation", "Elapsed us", "KiB/s", "Mbps", "TX pkt/B", "RX pkt/B"
    );

    let mut req_id = 12_000u64;
    let sizes: &[u64] = &[1024, 1400, 16 * 1024, 64 * 1024];
    let mut cur_baud = 115_200u32;
    let mut guard = RestoreBaud(serial_path.clone(), 115_200);

    for target_baud in bauds {
        {
            let mut p = XipPort::open_baud(serial_path.clone(), Some(cur_baud)).expect("open serial");
            p.wake().expect("wake before baud switch");
            p.negotiate_baud(target_baud).expect("negotiate baud");
            assert!(p.try_wake(Duration::from_secs(2)).expect("wake after baud switch"));
            cur_baud = target_baud;
            guard.1 = cur_baud;
        }
        let mut ctrl_req_id = 90_000u64;
        let mut req_at = |component: u64, method: u64, fields: &[u8]| -> Result<Vec<u8>, String> {
            let id = ctrl_req_id;
            ctrl_req_id += 1;
            let mut ebuf = [0u8; 256];
            let mut ew = cbor::Writer::new(&mut ebuf);
            ew.map(4).uint(1).uint(component).uint(2).uint(method).uint(3).uint(id).uint(5).raw(fields);
            let env = ew.finish().ok_or("envelope too large")?;
            let wire = encode_record(PORT_QUIC, &wrap_request(env));
            let mut p = XipPort::open_baud(serial_path.clone(), Some(target_baud)).map_err(|e| e.to_string())?;
            p.wake()?;
            let mut reader = Reader::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            p.write_all(&wire).map_err(|e| e.to_string())?;
            while Instant::now() < deadline {
                let bytes = p.read().map_err(|e| e.to_string())?;
                for ev in reader.feed(&bytes) {
                    if let Event::Record(PORT_QUIC, pkt) = ev {
                        if let Some(ans) = unwrap_answer(&pkt) {
                            if cbor::get_int(ans, 3) == Ok(Some(id as i64)) {
                                if let Ok(Some(err)) = cbor::get_int(ans, 7) {
                                    return Err(format!("native {component}.{method} err {err}"));
                                }
                                if let Ok(Some(mut r)) = cbor::get(ans, 6) {
                                    let rest = r.rest();
                                    if r.skip() {
                                        return Ok(rest[..rest.len() - r.rest().len()].to_vec());
                                    }
                                }
                            }
                        }
                    }
                }
                let _ = reader.take_text();
                if bytes.is_empty() {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(format!("timeout {component}.{method} at {target_baud}"))
        };

        for &(mode, label, serial_encrypt, set_secret) in &[
            (DeviceCryptoMode::Off, "crypto=OFF", false, false),
            (DeviceCryptoMode::PrekeyedGcm, "GCM(prekey)", true, true),
            (DeviceCryptoMode::PrekeyedCcm, "CCM(prekey)", true, true),
        ] {
            req_at(55, 2, &[0xa0]).expect("quic2.reset");
            let mut buf = [0u8; 96];
            let mut w = cbor::Writer::new(&mut buf);
            w.map(6)
                .uint(1).boolean(false)
                .uint(2).bytes(if set_secret { node_secret.as_slice() } else { &[] })
                .uint(3).bytes(&[])
                .uint(4).boolean(serial_encrypt)
                .uint(5).boolean(false)
                .uint(6).uint(mode.cipher() as u64);
            req_at(55, 3, w.finish().unwrap()).expect("quic2.config");
            {
                let mut cli = Quic2CryptoClient::new(
                    PacketIo::new_serial(&serial_path, PORT_QUIC2, Some(target_baud)),
                    1472,
                    true,
                    mode,
                    set_secret.then_some(node_secret),
                );
                for &sz in sizes {
                    for dir in [ProbeDirection::Upload, ProbeDirection::Download] {
                        req_id += 1;
                        let pkt_sz = (sz as usize).min(1024) as u16;
                        let mut req = ProbeServiceRequest::new(sz, pkt_sz);
                        req.direction = dir;
                        req.verify = true;
                        let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                        let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                        let upload = (dir == ProbeDirection::Upload)
                            .then(|| ProbePayload::new(sz, pkt_sz as usize).unwrap());
                        let reader = ProbeResponseReader::for_request(req);
                        let st = cli.execute_probe(&hdr[..n], upload, reader, false);
                        let op = if dir == ProbeDirection::Upload {
                            format!("up({sz})")
                        } else {
                            format!("dn({sz})")
                        };
                        println!(
                            "{:<12} {:<14} {:<12} {:>10} {:>8.2} {:>8.3} {:>4}/{:<6} {:>4}/{:<6}",
                            format!("{target_baud}"),
                            label,
                            op,
                            st.elapsed_us,
                            (sz as f64 / 1024.0) / (st.elapsed_us as f64 / 1e6),
                            (sz as f64 * 8.0) / (st.elapsed_us as f64),
                            st.tx_packets,
                            st.tx_wire_bytes,
                            st.rx_packets,
                            st.rx_wire_bytes
                        );
                    }
                }
            }
            let status = req_at(55, 1, &[0xa0]).expect("quic2.status");
            let errs = cbor::get_int(&status, 7).ok().flatten().unwrap_or(-1);
            let last_err = cbor::get_int(&status, 8).ok().flatten().unwrap_or(0);
            println!("  [quic2.status baud={target_baud} {label}] errors={errs} last_error={last_err}");
            assert_eq!(errs, 0, "unexpected mod_quic2 PPP errors at baud={target_baud} {label}");
        }
    }
    drop(guard);
}
