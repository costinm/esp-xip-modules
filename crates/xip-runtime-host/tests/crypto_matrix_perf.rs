mod common;
use common::module;
use dmesh_cli::mux_cli::{ProbeDirection, ProbeServiceRequest};
use quic_base::{
    AEAD_TAG_LEN, CryptoError, CryptoProvider, CryptoRole, PUBLIC_KEY_COMPRESSED_LEN,
    TRAFFIC_IV_LEN, TRAFFIC_KEY_LEN, TransportParameters, derive_device_secret_with,
    derive_peer_api_key_with,
};
use quic_link::{
    AssocConfig, AssocCryptoConfig, CongestionMode, ConnectionId, HandshakeMode, LinkFraming,
    LinkId, LinkProfile, QuicLinkAssoc,
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
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use xip_abi::{PKT_MAX, cbor};
use xip_runtime_host::{Config, Instance, NativeTx, Runtime, flash::FlashFile};

#[cfg(target_os = "linux")]
#[repr(C)]
struct BenchTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> u64 {
    const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
    unsafe extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut BenchTimespec) -> i32;
    }
    let mut ts = BenchTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc == 0 {
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    } else {
        0
    }
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_ns() -> u64 {
    0
}

static NEXT_FLASH_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_flash_image() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "crypto-matrix-flash-{}-{}.bin",
        std::process::id(),
        NEXT_FLASH_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
    let mut table = [255u8; 64];
    table[..2].copy_from_slice(&[0xaa, 0x50]);
    table[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
    table[8..12].copy_from_slice(&0x3f0000u32.to_le_bytes());
    table[12..16].copy_from_slice(b"xip\0");
    f.program(0x8000, &table).unwrap();
    path
}

fn attach_flash_and_load_modules(
    flash_path: &Path,
    with_udp: bool,
    with_serial: bool,
) -> (Runtime, Instance, Instance, Instance) {
    let rt = Runtime::new(Config {
        pool_slots: 64,
        inbox: 32,
        mem_quota: 131_072,
        ..Default::default()
    });
    rt.attach_flash(FlashFile::open(flash_path, 4 * 1024 * 1024).unwrap())
        .unwrap();
    rt.state_mut().running_flash = Some((0x10000, 0x180000));

    let rtest = rt.load_module(module("mod_rtest")).unwrap();
    rtest.start(b"probe");
    let flash = rt.load_module(module("mod_flash")).unwrap();
    flash.start(&[]);
    rt.run(30_000);
    assert!(!rtest.ended(), "rtest logs: {:?}", rtest.logs());
    assert!(!flash.ended(), "flash logs: {:?}", flash.logs());

    if with_udp {
        rt.add_native_link("udp", PKT_MAX, NativeTx::Done);
        rt.set_quic_classifier(true);
    }
    if with_serial {
        rt.add_native_link("serial", PKT_MAX, NativeTx::Done);
    }

    let quic2 = rt.load_module(module("mod_quic2")).unwrap();
    quic2.start(b"");
    rt.run(0);
    assert!(!quic2.ended(), "mod_quic2 logs: {:?}", quic2.logs());
    (rt, rtest, flash, quic2)
}

fn psa_sha256(_ctx: usize, chunks: &[&[u8]], out: &mut [u8; 32]) -> Result<(), CryptoError> {
    let mut h = xip_psa::Hasher::new(xip_psa::ALG_SHA_256).ok_or(CryptoError::Invalid)?;
    for chunk in chunks {
        h.update(chunk);
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

struct BenchStats {
    wall_us: u64,
    cpu_us: u64,
    tx_packets: u64,
    tx_wire_bytes: u64,
    rx_packets: u64,
    rx_wire_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientCryptoMode {
    Off,
    PrekeyedGcm,
    InitialGcm,
    PrekeyedCcm,
    InitialCcm,
}

impl ClientCryptoMode {
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

struct Quic2HostClient {
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    link_name: &'static str,
    token: u64,
    now_us: u64,
}

impl Quic2HostClient {
    fn new(
        link_name: &'static str,
        token: u64,
        mtu: usize,
        mode: ClientCryptoMode,
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
            max_association_rx_packets: 16,
            max_device_rx_packets: 24,
            gap_timeout_us: 3_000_000,
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
            max_retained_tx_packets: 24,
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 20_000,
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
                    max_inflight_packets: 64,
                    max_inflight_bytes: 64 * 1024,
                },
                framing: LinkFraming::Datagram { peer_cid: cid },
                initial_rtt_us: 5_000,
                encrypt: mode != ClientCryptoMode::Off,
            },
        );
        if mode != ClientCryptoMode::Off {
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
            link_name,
            token,
            now_us: 1_000,
        }
    }

    fn execute_probe(
        &mut self,
        rt: &Runtime,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        mut probe_reader: ProbeResponseReader,
        with_initial: bool,
    ) -> BenchStats {
        let pool_ref = self.pool_ref;
        let cpu0 = thread_cpu_ns();
        let started = Instant::now();

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
        let mut chunk_buf = [0u8; 1400];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;
        let mut rx_cb = Quic2ClientRx::default();

        for step in 0..10_000 {
            self.now_us += 100;

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
                assert_eq!(
                    rt.inject_frame(self.link_name, self.token, 0, out.packet.bytes()),
                    0
                );
                tx_packets += 1;
                moved = true;
            }
            if moved {
                rt.run(0);
            }

            let replies = rt.take_link_tx(self.link_name);
            if !replies.is_empty() {
                moved = true;
                for (_tok, wire) in replies {
                    rx_packets += 1;
                    rx_wire_bytes += wire.len() as u64;
                    let mut p = pool_ref.alloc().unwrap();
                    p.append(&wire).unwrap();
                    let demux = self
                        .assoc
                        .receive_packet(self.link_id, p, self.now_us)
                        .unwrap();
                    if let Some(mux_pkt) = demux.mux_packet {
                        self.mux
                            .receive_packet(
                                RxMeta {
                                    association: OWNER_ASSOC,
                                    received_at_us: self.now_us,
                                },
                                mux_pkt,
                                &mut self.assoc,
                                &mut rx_cb,
                            )
                            .unwrap();
                    }
                }
                if let Some((buf, _)) = rx_cb.streams.get_mut(&stream_id) {
                    if !buf.is_empty() {
                        probe_reader.push(buf).expect("valid probe response chunk");
                        buf.clear();
                    }
                }
            }

            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
            while let Some(out) = self.assoc.pop_outbound_packet() {
                tx_wire_bytes += out.packet.bytes().len() as u64;
                assert_eq!(
                    rt.inject_frame(self.link_name, self.token, 0, out.packet.bytes()),
                    0
                );
                tx_packets += 1;
                moved = true;
            }
            if moved {
                rt.run(0);
            }

            let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
            if tx_fin_sent && rx_fin {
                break;
            }

            if !moved {
                self.now_us += 5_000;
                let mut pr = pool_ref;
                let _ = self.assoc.poll_timers(&mut pr, self.now_us);
                let _ = self
                    .mux
                    .advance_time(self.now_us, &mut self.assoc, &mut rx_cb);
                while let Some(out) = self.assoc.pop_outbound_packet() {
                    tx_wire_bytes += out.packet.bytes().len() as u64;
                    assert_eq!(
                        rt.inject_frame(self.link_name, self.token, 0, out.packet.bytes()),
                        0
                    );
                    tx_packets += 1;
                }
                rt.run(5);
            }
            assert!(
                step < 9_999,
                "Quic2HostClient probe timed out (hs_done={}, tx_fin_sent={}, rx_fin={}, tx_pkts={}, rx_pkts={})",
                self.assoc.is_handshake_complete(),
                tx_fin_sent,
                rx_fin,
                tx_packets,
                rx_packets
            );
        }

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        self.now_us += 30_000;
        let mut pr = pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, self.now_us);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            tx_wire_bytes += ack.packet.bytes().len() as u64;
            assert_eq!(
                rt.inject_frame(self.link_name, self.token, 0, ack.packet.bytes()),
                0
            );
            tx_packets += 1;
        }
        rt.run(0);
        let _ = rt.take_link_tx(self.link_name);

        if let Some((rem, _)) = rx_cb.streams.remove(&stream_id) {
            if !rem.is_empty() {
                probe_reader.push(&rem).expect("valid final probe response");
            }
        }
        assert!(probe_reader.is_complete(), "probe incomplete at stream FIN");

        BenchStats {
            wall_us: started.elapsed().as_micros() as u64,
            cpu_us: thread_cpu_ns().saturating_sub(cpu0) / 1_000,
            tx_packets,
            tx_wire_bytes,
            rx_packets,
            rx_wire_bytes,
        }
    }
}

fn configure_quic2_module(
    quic2: &Instance,
    udp_encrypt: bool,
    serial_encrypt: bool,
    cipher: u8,
    node_secret: Option<[u8; 32]>,
) {
    let mut buf = [0u8; 64];
    let mut w = cbor::Writer::new(&mut buf);
    if let Some(ns) = node_secret {
        w.map(4)
            .uint(1)
            .boolean(udp_encrypt)
            .uint(2)
            .bytes(&ns)
            .uint(4)
            .boolean(serial_encrypt)
            .uint(6)
            .uint(cipher as u64);
    } else {
        w.map(4)
            .uint(1)
            .boolean(udp_encrypt)
            .uint(2)
            .bytes(&[])
            .uint(4)
            .boolean(serial_encrypt)
            .uint(6)
            .uint(cipher as u64);
    }
    let req = w.finish().unwrap();
    let resp = quic2.request(55, 3, req).unwrap();
    assert_eq!(
        cbor::get(&resp, 1).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Bool(udp_encrypt))
    );
    assert_eq!(
        cbor::get(&resp, 3).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Bool(serial_encrypt))
    );
    assert_eq!(
        cbor::get(&resp, 5).unwrap().and_then(|mut r| r.next()),
        Some(cbor::Item::Uint(cipher as u64))
    );
}

#[test]
#[ignore]
fn bench_mod_quic2_wasm_crypto_matrix() {
    let flash_path = fresh_flash_image();
    let prov = host_psa_provider();
    let pub65 = xip_psa::identity_public_key();
    let mut esp_pub = [0u8; PUBLIC_KEY_COMPRESSED_LEN];
    esp_pub[0] = 2 | (pub65[64] & 1);
    esp_pub[1..].copy_from_slice(&pub65[1..33]);
    let node_secret =
        derive_device_secret_with(&prov, b"wasm-matrix-root-secret-material", &esp_pub).unwrap();

    println!(
        "=== xip-runtime-host WASM mod_quic2 + mod_rtest Crypto OFF vs ON (host x86_64 release) ==="
    );
    println!(
        "{:<10} {:<20} {:<10} {:>10} {:>10} {:>9} {:>9} {:>11} {:>11}",
        "Bearer", "Mode", "Operation", "Wall us", "CPU us", "MiB/s", "Mbps", "TX pkt/B", "RX pkt/B"
    );

    let sizes: &[u64] = &[64, 256, 1024, 1400, 64 * 1024, 256 * 1024, 1024 * 1024, 2 * 1024 * 1024];
    let mut req_id = 1000u64;

    let cases: &[(&str, &str, u64, usize, bool, ClientCryptoMode, &str)] = &[
        ("PPP(243)", "serial", 243u64, 1472usize, true, ClientCryptoMode::Off, "crypto=OFF"),
        ("PPP(243)", "serial", 243u64, 1472usize, true, ClientCryptoMode::PrekeyedGcm, "GCM(prekey)"),
        ("PPP(243)", "serial", 243u64, 1472usize, true, ClientCryptoMode::PrekeyedCcm, "CCM(prekey)"),
        ("UDP(1472)", "udp", 9u64, 1472usize, false, ClientCryptoMode::Off, "crypto=OFF"),
        ("UDP(1472)", "udp", 9u64, 1472usize, false, ClientCryptoMode::PrekeyedGcm, "GCM(prekey)"),
        ("UDP(1472)", "udp", 9u64, 1472usize, false, ClientCryptoMode::PrekeyedCcm, "CCM(prekey)"),
        ("UDP(1472)", "udp", 9u64, 1472usize, false, ClientCryptoMode::InitialGcm, "GCM(initial)"),
        ("UDP(1472)", "udp", 9u64, 1472usize, false, ClientCryptoMode::InitialCcm, "CCM(initial)"),
    ];

    for &(bearer_label, link_name, token, mtu, is_serial, mode, label) in cases {
        let (rt, rtest, flash, quic2_mod) =
            attach_flash_and_load_modules(&flash_path, !is_serial, is_serial);
        let enc_on = mode != ClientCryptoMode::Off;
        configure_quic2_module(
            &quic2_mod,
            !is_serial && enc_on,
            is_serial && enc_on,
            mode.cipher(),
            enc_on.then_some(node_secret),
        );

        let mut cli = if mode.is_initial() {
            None
        } else {
            Some(Quic2HostClient::new(
                link_name,
                token,
                mtu,
                mode,
                (mode != ClientCryptoMode::Off).then_some(node_secret),
            ))
        };

        for &sz in sizes {
            if mode.is_initial() && sz > 1024 * 1024 {
                continue;
            }
            for dir in [ProbeDirection::Upload, ProbeDirection::Download] {
                if mode.is_initial() {
                    let _ = quic2_mod.request(55, 2, &[0xa0]).unwrap();
                }
                let mut local_cli;
                let c: &mut Quic2HostClient = if let Some(existing) = cli.as_mut() {
                    existing
                } else {
                    local_cli = Quic2HostClient::new(
                        link_name,
                        token,
                        mtu,
                        mode,
                        Some(node_secret),
                    );
                    &mut local_cli
                };

                req_id += 1;
                let chunk_sz = (sz as usize).min(1024);
                let mut req = ProbeServiceRequest::new(sz, chunk_sz as u16);
                req.direction = dir;
                req.verify = true;
                let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                let n = encode_probe_handler_request(req, req_id, &mut hdr).unwrap();
                let upload =
                    (dir == ProbeDirection::Upload).then(|| ProbePayload::new(sz, chunk_sz).unwrap());
                let reader = ProbeResponseReader::for_request(req);

                let stats = c.execute_probe(
                    &rt,
                    &hdr[..n],
                    upload,
                    reader,
                    mode.is_initial(),
                );
                let mibs = (sz as f64 * 1e6) / ((stats.wall_us.max(1) as f64) * 1024.0 * 1024.0);
                let mbps = (sz as f64 * 8.0) / (stats.wall_us.max(1) as f64);
                let op = if dir == ProbeDirection::Upload {
                    format!("up({sz})")
                } else {
                    format!("dn({sz})")
                };
                println!(
                    "{:<10} {:<20} {:<10} {:>10} {:>10} {:>9.2} {:>9.2} {:>4}/{:<6} {:>4}/{:<6}",
                    bearer_label,
                    label,
                    op,
                    stats.wall_us,
                    stats.cpu_us,
                    mibs,
                    mbps,
                    stats.tx_packets,
                    stats.tx_wire_bytes,
                    stats.rx_packets,
                    stats.rx_wire_bytes
                );
            }
        }

        let status = quic2_mod.request(55, 1, &[0xa0]).unwrap();
        assert_eq!(cbor::get_int(&status, 7).unwrap(), Some(0));

        quic2_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
        assert_eq!(quic2_mod.leaked(), 0);
        assert_eq!(quic2_mod.mem_leaked(), 0);
        assert_eq!(rtest.leaked(), 0);
        assert_eq!(flash.leaked(), 0);
        assert_eq!(rt.packets_in_use(), 0);
    }

    let _ = std::fs::remove_file(&flash_path);
}
