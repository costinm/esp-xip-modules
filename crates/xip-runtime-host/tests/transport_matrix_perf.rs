//! Host-runtime (`xip-runtime-host`) transport-independence verification and performance
//! benchmarks across all 5 transport options:
//! 1. Plain TCP (single stream, `--stream 3` for `mod_rtest`, `--stream 2` for `mod_flash`)
//! 2. Original `quic-lite` (`mod_quic`, component `48`, over UDP and PPP serial)
//! 3. `quic-mux` only (`mod_mux`, component `52`, over TCP `--stream 4` and PPP serial `--port 245`)
//! 4. `quic-link` standalone single-stream (`mod_qlink`, component `54`, over UDP and PPP serial `--port 242`)
//! 5. `quic-link` + `quic-mux` combined (`mod_quic2`, component `55`, over UDP, PPP serial `--port 243`, and TCP `--stream 6`)
//!
//! Verifies that the exact same Wasm consumer module binaries (`mod_rtest.wasm` and
//! `mod_flash.wasm`) serve requests without any transport-specific consumer code changes,
//! and records performance numbers for `1 KiB` (`1,024 B`), `1 MiB` (`1,048,576 B`), and
//! `2 MiB` (`2,097,152 B`) transfers.

mod common;
use common::module;
use dmesh_cli::mux_cli::{
    MuxClientTarget, PPP_MUX_MTU, ProbeDirection, ProbeServiceRequest, TokioMux, TokioMuxConfig,
    connect_mux_client, mux_exchange, mux_probe,
};
use quic_tools::{ProbeResponseReader, encode_probe_handler_request};
use quic_base::wire::StreamPacketPlan;
use quic_link::{
    AssocConfig, CongestionMode, ConnectionId, LinkFraming, LinkId, LinkProfile, LinkTxOutcome,
    QuicLinkAssoc, SingleStreamCallbacks, SingleStreamConfig, SingleStreamEndpoint,
    flow::{RxOutcome as LinkRxOutcome, StreamPriority},
    link::OWNER_ASSOC,
    pool::{PacketPool as LinkPacketPool, PoolPacket as LinkPoolPacket, StaticPacketLease, StaticPacketPool},
    streams::{RxStreamChunk as LinkRxStreamChunk, TxStreamData as LinkTxStreamData},
    wire::plan_short_header_stream_packet,
};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, QuicMux, RxMeta,
    RxOutcome as MuxRxOutcome, RxStreamChunk as MuxRxStreamChunk, StreamCloseReason, StreamError,
    StreamId, TxStreamData as MuxTxStreamData,
};
use quic_tools::probe::ProbePayload;
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::mpsc,
};
use xip_abi::{PKT_MAX, cbor};
use xip_runtime_host::{Config, Instance, NativeTx, Runtime, flash::FlashFile};

static NEXT_FLASH_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_flash_image() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "transport-matrix-flash-{}-{}.bin",
        std::process::id(),
        NEXT_FLASH_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
    let mut table = [255u8; 64];
    table[..2].copy_from_slice(&[0xaa, 0x50]);
    table[2] = 0;
    table[3] = 0;
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
    serial_quic: bool,
    transport_module: Option<&str>,
) -> (Runtime, Instance, Instance, Option<Instance>) {
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
        if serial_quic {
            rt.set_quic_classifier(true);
            rt.quic_link_enable("serial");
        }
    }

    let tmod = transport_module.map(|name| {
        let m = rt.load_module(module(name)).unwrap();
        m.start(b"");
        rt.run(0);
        assert!(!m.ended(), "{name} logs: {:?}", m.logs());
        m
    });

    (rt, rtest, flash, tmod)
}

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

/// Parse the response from `mod_flash` stream (`write_pkt` returns the raw CBOR map without an
/// outer envelope, whereas `tcp.rs` also writes the raw CBOR map `{1: version, 2: size, ...}`
/// followed by the raw file bytes on `file.read`).
fn split_flash_stream_response(resp: &[u8]) -> (&[u8], &[u8]) {
    let mut r = cbor::Reader::new(resp);
    assert!(r.skip(), "expected valid CBOR map header at start of flash stream response");
    let body = r.rest();
    let hdr_len = resp.len() - body.len();
    (&resp[..hdr_len], body)
}

#[derive(Clone, Debug)]
struct TransferStats {
    elapsed_us: u64,
    tx_packets: u64,
    rx_packets: u64,
}

impl TransferStats {
    fn mib_per_sec(&self, bytes: usize) -> f64 {
        if self.elapsed_us == 0 {
            0.0
        } else {
            (bytes as f64 / (1024.0 * 1024.0)) / (self.elapsed_us as f64 / 1_000_000.0)
        }
    }
    fn mbps(&self, bytes: usize) -> f64 {
        if self.elapsed_us == 0 {
            0.0
        } else {
            (bytes as f64 * 8.0) / (self.elapsed_us as f64)
        }
    }
}

// ============================================================================
// Driver 1: `mod_qlink` (`quic-link` standalone single-stream over UDP or PPP)
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

struct QlinkHostClient {
    endpoint: SingleStreamEndpoint<&'static StaticPacketPool<32>>,
    link_id: LinkId,
    link_name: &'static str,
    token: u64,
    next_stream_idx: u64,
    now_us: u64,
}

impl QlinkHostClient {
    fn new(link_name: &'static str, token: u64, mtu: usize) -> Self {
        let pool_ref: &'static StaticPacketPool<32> =
            Box::leak(Box::new(StaticPacketPool::<32>::new()));
        let cid = ConnectionId::new(1).unwrap();
        let cfg = SingleStreamConfig {
            stream_id: StreamId(0),
            stream_priority: StreamPriority::NORMAL,
            initial_recv_window: 64 * 1024,
            initial_send_window: 64 * 1024,
            max_reassembly_chunks: 16,
            assoc: AssocConfig {
                assoc_id: OWNER_ASSOC,
                local_cid: None,
                default_peer_cid: cid,
                max_retained_tx_packets: 16,
                ack_eliciting_threshold: 2,
                max_ack_delay_us: 20_000,
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
                    max_inflight_packets: 64,
                    max_inflight_bytes: 64 * 1024,
                },
                framing: LinkFraming::Datagram { peer_cid: cid },
                initial_rtt_us: 5_000,
                encrypt: false,
            },
        );
        Self {
            endpoint,
            link_id,
            link_name,
            token,
            next_stream_idx: 0,
            now_us: 1_000,
        }
    }

    fn prepare_next_stream(&mut self) {
        let sid = StreamId(self.next_stream_idx * 4);
        self.next_stream_idx += 1;
        if sid.0 != 0 {
            self.endpoint.reset_stream_state(sid);
        }
    }

    fn execute_stream(
        &mut self,
        rt: &Runtime,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        self.prepare_next_stream();
        let started = Instant::now();
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1400];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;

        let mut collector = QlinkClientCollector::default();

        for step in 0..2_000_000 {
            self.now_us += 100;
            // 1. Write as much request data as flow control & SACK ledger allow.
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
                        self.now_us,
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
                                .write_stream(LinkTxStreamData::Bytes(&[]), true, self.now_us)
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
                        self.now_us,
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
                        self.now_us,
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
                        .write_stream(LinkTxStreamData::Bytes(&[]), true, self.now_us)
                        .is_ok()
                    {
                        tx_fin_sent = true;
                    }
                    break;
                }
            }

            // 2. Pop outbound packets and inject into runtime.
            let mut moved = false;
            while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
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

            // 3. Drain packets sent by the runtime.
            let replies = rt.take_link_tx(self.link_name);
            if !replies.is_empty() {
                moved = true;
                for (_tok, wire) in replies {
                    rx_packets += 1;
                    let mut p = self.endpoint.alloc_packet().unwrap();
                    p.append(&wire).unwrap();
                    self.endpoint
                        .receive_packet(self.link_id, p, self.now_us, &mut collector)
                        .unwrap();
                }
                if let Some(reader) = probe_reader.as_deref_mut() {
                    if !collector.bytes.is_empty() {
                        reader.push(&collector.bytes).expect("valid probe response chunk");
                        collector.bytes.clear();
                    }
                }
            }

            // 4. Flush any immediate ACKs or window updates back to runtime.
            while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
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

            if tx_fin_sent && collector.fin {
                break;
            }

            if !moved {
                self.now_us += 5_000;
                let _ = self.endpoint.poll_timers(self.now_us);
                while let Some(out) = self.endpoint.assoc_mut().pop_outbound_packet() {
                    assert_eq!(
                        rt.inject_frame(self.link_name, self.token, 0, out.packet.bytes()),
                        0
                    );
                    tx_packets += 1;
                }
                rt.run(5);
            }
            assert!(step < 1_999_999, "QlinkHostClient stream exchange timed out");
        }

        // Flush final ACK so module releases retained SACK packets.
        self.now_us += 30_000;
        let _ = self.endpoint.poll_timers(self.now_us);
        while let Some(ack) = self.endpoint.assoc_mut().pop_outbound_packet() {
            assert_eq!(
                rt.inject_frame(self.link_name, self.token, 0, ack.packet.bytes()),
                0
            );
            tx_packets += 1;
        }
        rt.run(0);
        let _ = rt.take_link_tx(self.link_name);

        if let Some(reader) = probe_reader {
            if !collector.bytes.is_empty() {
                reader.push(&collector.bytes).expect("valid final probe response");
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
// Driver 2: `mod_quic2` (`quic-link` + `quic-mux` over UDP, PPP, or TCP)
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

struct Quic2LinkHostClient {
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    link_name: &'static str,
    token: u64,
    now_us: u64,
}

impl Quic2LinkHostClient {
    fn new(link_name: &'static str, token: u64, mtu: usize) -> Self {
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
        let mut assoc = QuicLinkAssoc::new(AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: None,
            default_peer_cid: cid,
            max_retained_tx_packets: 24,
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 20_000,
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
                encrypt: false,
            },
        );
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

    fn execute_stream(
        &mut self,
        rt: &Runtime,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        let pool_ref = self.pool_ref;
        let stream_id = self.mux.open_stream(OWNER_ASSOC, true).unwrap();
        let started = Instant::now();
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1400];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;

        let mut rx_cb = Quic2ClientRx::default();

        for step in 0..2_000_000 {
            self.now_us += 100;

            // 1. Write stream data while capacity & SACK ledger allow.
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

            // 2. Pop outbound packets and inject into runtime.
            let mut moved = false;
            while let Some(out) = self.assoc.pop_outbound_packet() {
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

            // 3. Drain packets sent by the runtime.
            let replies = rt.take_link_tx(self.link_name);
            if !replies.is_empty() {
                moved = true;
                for (_tok, wire) in replies {
                    rx_packets += 1;
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
                if let Some(reader) = probe_reader.as_deref_mut() {
                    if let Some((buf, _)) = rx_cb.streams.get_mut(&stream_id) {
                        if !buf.is_empty() {
                            reader.push(buf).expect("valid probe response chunk");
                            buf.clear();
                        }
                    }
                }
            }

            // 4. Flush any immediate ACKs or MAX_DATA / MAX_STREAM_DATA frames back.
            let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
            while let Some(out) = self.assoc.pop_outbound_packet() {
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
                let _ = self.mux.advance_time(self.now_us, &mut self.assoc, &mut rx_cb);
                while let Some(out) = self.assoc.pop_outbound_packet() {
                    assert_eq!(
                        rt.inject_frame(self.link_name, self.token, 0, out.packet.bytes()),
                        0
                    );
                    tx_packets += 1;
                }
                rt.run(5);
            }
            assert!(step < 1_999_999, "Quic2LinkHostClient stream exchange timed out");
        }

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        self.now_us += 30_000;
        let mut pr = pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, self.now_us);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            assert_eq!(
                rt.inject_frame(self.link_name, self.token, 0, ack.packet.bytes()),
                0
            );
            tx_packets += 1;
        }
        rt.run(0);
        let _ = rt.take_link_tx(self.link_name);

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

/// Persistent async TCP client for `mod_quic2` (`--stream 6`, self-delimiting ShortHeader + SACK + `QuicMux`).
struct Quic2TcpHostClient {
    tcp: TcpStream,
    pool_ref: &'static StaticPacketPool<32>,
    mux: QuicMux<&'static StaticPacketPool<32>>,
    assoc: QuicLinkAssoc<StaticPacketLease<'static, 32>>,
    link_id: LinkId,
    rx_wire_buf: Vec<u8>,
    now_us: u64,
}

impl Quic2TcpHostClient {
    async fn connect(tcp_addr: SocketAddr) -> Self {
        let tcp = TcpStream::connect(tcp_addr).await.expect("connect mod_quic2 TCP");
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
            max_association_rx_packets: 16,
            max_device_rx_packets: 24,
            gap_timeout_us: 3_000_000,
        };
        let mut mux = QuicMux::new(pool_ref, limits);
        mux.add_association(OWNER_ASSOC, true).unwrap();

        let cid = ConnectionId::new(1).unwrap();
        let mut assoc = QuicLinkAssoc::new(AssocConfig {
            assoc_id: OWNER_ASSOC,
            local_cid: None,
            default_peer_cid: cid,
            max_retained_tx_packets: 24,
            ack_eliciting_threshold: 2,
            max_ack_delay_us: 20_000,
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
                    max_inflight_packets: 64,
                    max_inflight_bytes: 64 * 1024,
                },
                framing: LinkFraming::SelfDelimitingStream { peer_cid: cid },
                initial_rtt_us: 2_000,
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
            now_us: 1_000,
        }
    }

    async fn execute_stream(
        &mut self,
        rt: &Runtime,
        header: &[u8],
        mut upload_payload: Option<ProbePayload>,
        raw_body: &[u8],
        mut probe_reader: Option<&mut ProbeResponseReader>,
    ) -> (Vec<u8>, TransferStats) {
        let stream_id = self.mux.open_stream(OWNER_ASSOC, true).unwrap();
        let started = Instant::now();
        let mut tx_packets = 0u64;
        let mut rx_packets = 0u64;

        let mut hdr_at = 0usize;
        let mut raw_at = 0usize;
        let mut tx_fin_sent = false;
        let mut chunk_buf = [0u8; 1400];
        let mut chunk_len = 0usize;
        let mut chunk_at = 0usize;

        let mut rx_cb = Quic2ClientRx::default();
        let mut read_tmp = [0u8; 2048];

        loop {
            self.now_us += 200;
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
                    self.now_us,
                );
                tx_packets += 1;
            }

            let rx_fin = rx_cb.streams.get(&stream_id).is_some_and(|(_, fin)| *fin);
            if tx_fin_sent && rx_fin {
                break;
            }

            match self.tcp.try_read(&mut read_tmp) {
                Ok(n) if n > 0 => {
                    self.rx_wire_buf.extend_from_slice(&read_tmp[..n]);
                    loop {
                        match plan_short_header_stream_packet(&self.rx_wire_buf, 1500) {
                            Ok(StreamPacketPlan::Ready { total_len }) => {
                                if self.rx_wire_buf.len() < total_len {
                                    break;
                                }
                                rx_packets += 1;
                                let mut p = self.pool_ref.alloc().unwrap();
                                p.append(&self.rx_wire_buf[..total_len]).unwrap();
                                self.rx_wire_buf.drain(..total_len);
                                let demux =
                                    self.assoc.receive_packet(self.link_id, p, self.now_us).unwrap();
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
                    let _ = self.assoc.poll_timers(&mut pr, self.now_us);
                    let _ = self.mux.flush_control(OWNER_ASSOC, &mut self.assoc);
                }
                _ => {
                    self.now_us += 5_000;
                    let mut pr = self.pool_ref;
                    let _ = self.assoc.poll_timers(&mut pr, self.now_us);
                    let _ = self.mux.advance_time(self.now_us, &mut self.assoc, &mut rx_cb);
                    rt.run(5);
                    tokio::task::yield_now().await;
                }
            }
        }

        let _ = self.mux.close_stream(OWNER_ASSOC, stream_id, &mut self.assoc);
        self.now_us += 30_000;
        let mut pr = self.pool_ref;
        let _ = self.assoc.poll_timers(&mut pr, self.now_us);
        while let Some(ack) = self.assoc.pop_outbound_packet() {
            let _ = self.tcp.write_all(ack.packet.bytes()).await;
            self.assoc.on_driver_complete(
                self.link_id,
                ack.packet_number,
                LinkTxOutcome::Sent,
                self.now_us,
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
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ============================================================================
// Test 1: Transport-Agnostic Consumer Verification (`mod_rtest` + `mod_flash`)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn transport_agnostic_rtest_and_flash_across_all_5_transports() {
    let flash_path = fresh_flash_image();
    let payload_1k: Vec<u8> = (0..1024).map(|i| ((i * 37 + 11) & 0xff) as u8).collect();

    // ------------------------------------------------------------------------
    // Transport 1: Plain TCP (`mod_rtest` `--stream 3`, `mod_flash` `--stream 2`)
    // ------------------------------------------------------------------------
    {
        let (rt, rtest, flash, _) =
            attach_flash_and_load_modules(&flash_path, false, false, false, None);
        let rtest_addr = rtest
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();
        let flash_addr = flash
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let payload_ref = &payload_1k;
        let client = async move {
            for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
                .into_iter()
                .enumerate()
            {
                let mut req = ProbeServiceRequest::new(1024, 512);
                req.direction = dir;
                req.verify = true;
                let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                let n = encode_probe_handler_request(req, 10 + idx as u64, &mut hdr).unwrap();
                let mut sock = TcpStream::connect(rtest_addr).await.unwrap();
                let res = quic_tools::stream_client::probe(&mut sock, &hdr[..n], req)
                    .await
                    .unwrap();
                assert_eq!(res.bytes, 1024);
            }

            // Write `f_tcp` via Plain TCP to `mod_flash`
            let whdr = encode_flash_write_header(b"f_tcp", payload_ref.len(), 20);
            let mut wsock = TcpStream::connect(flash_addr).await.unwrap();
            let wresp =
                quic_tools::stream_client::exchange(&mut wsock, &[&whdr, payload_ref])
                    .await
                    .unwrap();
            let (wmap, wbody) = split_flash_stream_response(&wresp);
            assert!(wbody.is_empty());
            assert_eq!(cbor::get_int(wmap, 1).unwrap(), Some(1));

            // Read `f_tcp` back via Plain TCP
            let rhdr = encode_flash_read_header(b"f_tcp", 21);
            let mut rsock = TcpStream::connect(flash_addr).await.unwrap();
            let rresp = quic_tools::stream_client::exchange(&mut rsock, &[&rhdr])
                .await
                .unwrap();
            let (rmap, rbody) = split_flash_stream_response(&rresp);
            assert_eq!(cbor::get_int(rmap, 2).unwrap(), Some(1024));
            assert_eq!(rbody, payload_ref.as_slice());
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(10)) => panic!("Plain TCP test timed out"),
                    _ = client => {},
                }
            })
            .await;

        rtest.stop();
        flash.stop();
        rt.run(0);
        assert_eq!(rtest.leaked(), 0);
        assert_eq!(flash.leaked(), 0);
        assert_eq!(rt.packets_in_use(), 0);
    }

    // ------------------------------------------------------------------------
    // Transport 3: `quic-mux` only (`mod_mux` `52` over TCP `--stream 4` & PPP `245`)
    // ------------------------------------------------------------------------
    {
        let (rt, rtest, flash, tmod) =
            attach_flash_and_load_modules(&flash_path, false, true, false, Some("mod_mux"));
        let mux_mod = tmod.unwrap();
        let tcp_addr = mux_mod
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let rt_serial = rt.clone();
        let payload_ref = &payload_1k;
        let client = async move {
            // (a) Over TCP `--stream 4`
            let target = MuxClientTarget {
                tcp: Some(tcp_addr),
                ..Default::default()
            };
            let (cli_mux, _acc, _guard) = connect_mux_client(&target, TokioMuxConfig::default())
                .await
                .unwrap();

            for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
                .into_iter()
                .enumerate()
            {
                let mut req = ProbeServiceRequest::new(1024, 512);
                req.direction = dir;
                req.verify = true;
                let res = mux_probe(&cli_mux, req, 200 + idx as u64).await.unwrap();
                assert_eq!(res.bytes, 1024);
            }

            // Read `f_tcp` written over Plain TCP, then write `f_mux_t` over `mod_mux` TCP!
            let rhdr_prev = encode_flash_read_header(b"f_tcp", 210);
            let rresp_prev = mux_exchange(&cli_mux, &rhdr_prev).await.unwrap();
            assert_eq!(split_flash_stream_response(&rresp_prev).1, payload_ref.as_slice());

            let mut wreq = encode_flash_write_header(b"f_mux_t", payload_ref.len(), 211);
            wreq.extend_from_slice(payload_ref);
            let wresp = mux_exchange(&cli_mux, &wreq).await.unwrap();
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
                Some(1)
            );

            let rhdr = encode_flash_read_header(b"f_mux_t", 212);
            let rresp = mux_exchange(&cli_mux, &rhdr).await.unwrap();
            assert_eq!(split_flash_stream_response(&rresp).1, payload_ref.as_slice());
            cli_mux.close();
            drop(cli_mux);

            // (b) Over PPP serial `--port 245`
            let (ser_mux, _ser_acc) = TokioMux::spawn(true, TokioMuxConfig::default());
            let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
            let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
            ser_mux.add_packet_link(out_tx, in_rx, PPP_MUX_MTU, 20);

            let stop = Arc::new(AtomicBool::new(false));
            let stop_bg = stop.clone();
            let rt_pump = rt_serial.clone();
            let bridge = tokio::task::spawn_local(async move {
                while !stop_bg.load(Ordering::Acquire) {
                    let mut moved = false;
                    while let Ok(frame) = out_rx.try_recv() {
                        assert_eq!(rt_pump.inject_frame("serial", 245, 0, &frame), 0);
                        rt_pump.run(0);
                        moved = true;
                    }
                    for (_tok, reply) in rt_pump.take_link_tx("serial") {
                        if in_tx.send(reply).await.is_err() {
                            return;
                        }
                        moved = true;
                    }
                    if !moved {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            });

            for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
                .into_iter()
                .enumerate()
            {
                let mut req = ProbeServiceRequest::new(1024, 512);
                req.direction = dir;
                req.verify = true;
                let res = mux_probe(&ser_mux, req, 250 + idx as u64).await.unwrap();
                assert_eq!(res.bytes, 1024);
            }

            let mut wreq_s = encode_flash_write_header(b"f_mux_s", payload_ref.len(), 260);
            wreq_s.extend_from_slice(payload_ref);
            let wresp_s = mux_exchange(&ser_mux, &wreq_s).await.unwrap();
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp_s).0, 1).unwrap(),
                Some(1)
            );
            let rresp_s = mux_exchange(&ser_mux, &encode_flash_read_header(b"f_mux_s", 261))
                .await
                .unwrap();
            assert_eq!(split_flash_stream_response(&rresp_s).1, payload_ref.as_slice());

            ser_mux.close();
            stop.store(true, Ordering::Release);
            let _ = bridge.await;
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(10)) => panic!("mod_mux test timed out"),
                    _ = client => {},
                }
            })
            .await;

        mux_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
        assert_eq!(mux_mod.leaked(), 0);
        assert_eq!(mux_mod.mem_leaked(), 0);
        assert_eq!(rtest.leaked(), 0);
        assert_eq!(flash.leaked(), 0);
        assert_eq!(rt.packets_in_use(), 0);
    }

    // ------------------------------------------------------------------------
    // Transport 4: `quic-link` standalone single-stream (`mod_qlink` `54` over UDP & PPP `242`)
    // ------------------------------------------------------------------------
    for (link_name, token, mtu, is_serial) in [
        ("udp", 7u64, 1472usize, false),
        ("serial", 242u64, 1500usize, true),
    ] {
        let (rt, rtest, flash, tmod) = attach_flash_and_load_modules(
            &flash_path,
            !is_serial,
            is_serial,
            false,
            Some("mod_qlink"),
        );
        let qlink_mod = tmod.unwrap();
        let mut cli = QlinkHostClient::new(link_name, token, mtu);

        for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
            .into_iter()
            .enumerate()
        {
            let mut req = ProbeServiceRequest::new(1024, 512);
            req.direction = dir;
            req.verify = true;
            let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
            let n = encode_probe_handler_request(req, 300 + idx as u64, &mut hdr).unwrap();
            let upload = (dir == ProbeDirection::Upload)
                .then(|| ProbePayload::new(1024, 512).unwrap());
            let mut reader = ProbeResponseReader::for_request(req);
            cli.execute_stream(&rt, &hdr[..n], upload, &[], Some(&mut reader));
            assert_eq!(reader.bytes() + reader.uploaded_bytes(), 1024);
        }

        // Read `f_mux_s` written over `mod_mux`, then write/read `f_qlink` over `mod_qlink`!
        let (rresp_prev, _) =
            cli.execute_stream(&rt, &encode_flash_read_header(b"f_mux_s", 310), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp_prev).1, payload_1k.as_slice());

        let fname = if is_serial { b"f_qk_s".as_slice() } else { b"f_qk_u".as_slice() };
        let whdr = encode_flash_write_header(fname, payload_1k.len(), 311);
        let (wresp, _) = cli.execute_stream(&rt, &whdr, None, &payload_1k, None);
        assert_eq!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
            Some(1)
        );

        let (rresp, _) = cli.execute_stream(&rt, &encode_flash_read_header(fname, 312), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());

        let status = qlink_mod.request(54, 1, &[0xa0]).unwrap();
        assert_eq!(cbor::get_int(&status, 5).unwrap(), Some(0)); // 0 errors

        qlink_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
        assert_eq!(qlink_mod.leaked(), 0);
        assert_eq!(qlink_mod.mem_leaked(), 0);
        assert_eq!(rtest.leaked(), 0);
        assert_eq!(flash.leaked(), 0);
        assert_eq!(rt.packets_in_use(), 0);
    }

    // ------------------------------------------------------------------------
    // Transport 5: `quic-link` + `quic-mux` combined (`mod_quic2` `55` over UDP, PPP `243`, & TCP `--stream 6`)
    // ------------------------------------------------------------------------
    for (link_name, token, mtu, is_serial) in [
        ("udp", 9u64, 1472usize, false),
        ("serial", 243u64, 1500usize, true),
    ] {
        let (rt, rtest, flash, tmod) = attach_flash_and_load_modules(
            &flash_path,
            !is_serial,
            is_serial,
            false,
            Some("mod_quic2"),
        );
        let quic2_mod = tmod.unwrap();
        let mut cli = Quic2LinkHostClient::new(link_name, token, mtu);

        for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
            .into_iter()
            .enumerate()
        {
            let mut req = ProbeServiceRequest::new(1024, 512);
            req.direction = dir;
            req.verify = true;
            let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
            let n = encode_probe_handler_request(req, 400 + idx as u64, &mut hdr).unwrap();
            let upload = (dir == ProbeDirection::Upload)
                .then(|| ProbePayload::new(1024, 512).unwrap());
            let mut reader = ProbeResponseReader::for_request(req);
            cli.execute_stream(&rt, &hdr[..n], upload, &[], Some(&mut reader));
            assert_eq!(reader.bytes() + reader.uploaded_bytes(), 1024);
        }

        // Read `f_qk_s` written over `mod_qlink`, then write/read `f_q2_*` over `mod_quic2`!
        let (rresp_prev, _) =
            cli.execute_stream(&rt, &encode_flash_read_header(b"f_qk_s", 410), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp_prev).1, payload_1k.as_slice());

        let fname = if is_serial { b"f_q2_s".as_slice() } else { b"f_q2_u".as_slice() };
        let whdr = encode_flash_write_header(fname, payload_1k.len(), 411);
        let (wresp, _) = cli.execute_stream(&rt, &whdr, None, &payload_1k, None);
        assert_eq!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
            Some(1)
        );

        let (rresp, _) = cli.execute_stream(&rt, &encode_flash_read_header(fname, 412), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());

        let status = quic2_mod.request(55, 1, &[0xa0]).unwrap();
        assert_eq!(cbor::get_int(&status, 7).unwrap(), Some(0)); // 0 errors

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

    // `mod_quic2` over TCP `--stream 6`
    {
        let (rt, rtest, flash, tmod) =
            attach_flash_and_load_modules(&flash_path, false, false, false, Some("mod_quic2"));
        let quic2_mod = tmod.unwrap();
        let tcp_addr = quic2_mod
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let payload_ref = &payload_1k;
        let rt_tcp = rt.clone();
        let client = async move {
            let mut tcp_cli = Quic2TcpHostClient::connect(tcp_addr).await;
            for (idx, dir) in [ProbeDirection::Upload, ProbeDirection::Download]
                .into_iter()
                .enumerate()
            {
                let mut req = ProbeServiceRequest::new(1024, 512);
                req.direction = dir;
                req.verify = true;
                let mut hdr = [0u8; quic_tools::PROBE_REQUEST_MAX];
                let n = encode_probe_handler_request(req, 450 + idx as u64, &mut hdr).unwrap();
                let upload = (dir == ProbeDirection::Upload)
                    .then(|| ProbePayload::new(1024, 512).unwrap());
                let mut reader = ProbeResponseReader::for_request(req);
                tcp_cli
                    .execute_stream(&rt_tcp, &hdr[..n], upload, &[], Some(&mut reader))
                    .await;
                assert_eq!(reader.bytes() + reader.uploaded_bytes(), 1024);
            }

            let whdr = encode_flash_write_header(b"f_q2_t", payload_ref.len(), 460);
            let (wresp, _) = tcp_cli
                .execute_stream(&rt_tcp, &whdr, None, payload_ref, None)
                .await;
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
                Some(1)
            );

            let rhdr = encode_flash_read_header(b"f_q2_t", 461);
            let (rresp, _) = tcp_cli.execute_stream(&rt_tcp, &rhdr, None, &[], None).await;
            assert_eq!(split_flash_stream_response(&rresp).1, payload_ref.as_slice());
            tcp_cli.close().await;
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(10)) => panic!("mod_quic2 TCP test timed out"),
                    _ = client => {},
                }
            })
            .await;

        rt.run(0);
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

// ============================================================================
// Test 2: Host Runtime Performance Benchmarks (`1 KiB`, `1 MiB`, `2 MiB`)
// ============================================================================

#[derive(Clone, Debug)]
struct PerfRow {
    transport: &'static str,
    bearer: &'static str,
    operation: &'static str,
    bytes: u64,
    stats: TransferStats,
}

#[tokio::test(flavor = "current_thread")]
async fn transport_performance_benchmarks_1k_1m_2m() {
    let flash_path = fresh_flash_image();
    let payload_1k: Vec<u8> = (0..1024).map(|i| ((i * 37 + 11) & 0xff) as u8).collect();
    let sizes: [(u64, &'static str, &'static str); 3] = [
        (1024, "rtest.probe upload (1 KiB)", "rtest.probe download (1 KiB)"),
        (
            1024 * 1024,
            "rtest.probe upload (1 MiB)",
            "rtest.probe download (1 MiB)",
        ),
        (
            2 * 1024 * 1024,
            "rtest.probe upload (2 MiB)",
            "rtest.probe download (2 MiB)",
        ),
    ];
    let mut rows: Vec<PerfRow> = Vec::new();

    // ------------------------------------------------------------------------
    // 1. Plain TCP (`mod_rtest` `--stream 3`, `mod_flash` `--stream 2`)
    // ------------------------------------------------------------------------
    {
        let (rt, rtest, flash, _) =
            attach_flash_and_load_modules(&flash_path, false, false, false, None);
        let rtest_addr = rtest
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();
        let flash_addr = flash
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let payload_ref = &payload_1k;
        let mut tcp_rows = Vec::new();
        let rows_out = &mut tcp_rows;
        let rt_plain = rt.clone();
        let client = async move {
            let stop_plain = Arc::new(AtomicBool::new(false));
            let stop_plain_bg = stop_plain.clone();
            let plain_pump = tokio::task::spawn_local(async move {
                while !stop_plain_bg.load(Ordering::Acquire) {
                    rt_plain.run(5);
                    tokio::task::yield_now().await;
                }
            });

            let mut req_id = 1000u64;
            for (sz, up_label, down_label) in sizes {
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
                    let mut sock = TcpStream::connect(rtest_addr).await.unwrap();
                    let _ = sock.set_nodelay(true);
                    let res = quic_tools::stream_client::probe(&mut sock, &hdr[..n], req)
                        .await
                        .unwrap();
                    let elapsed_us = t0.elapsed().as_micros() as u64;
                    assert_eq!(res.bytes, sz);
                    rows_out.push(PerfRow {
                        transport: "1. Plain TCP",
                        bearer: "TCP",
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

            let whdr = encode_flash_write_header(b"p_tcp", payload_ref.len(), 1090);
            let t0 = Instant::now();
            let mut wsock = TcpStream::connect(flash_addr).await.unwrap();
            let _ = wsock.set_nodelay(true);
            let wresp =
                quic_tools::stream_client::exchange(&mut wsock, &[&whdr, payload_ref])
                    .await
                    .unwrap();
            let w_us = t0.elapsed().as_micros() as u64;
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
                Some(1)
            );
            rows_out.push(PerfRow {
                transport: "1. Plain TCP",
                bearer: "TCP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: TransferStats {
                    elapsed_us: w_us,
                    tx_packets: 0,
                    rx_packets: 0,
                },
            });

            let rhdr = encode_flash_read_header(b"p_tcp", 1091);
            let t1 = Instant::now();
            let mut rsock = TcpStream::connect(flash_addr).await.unwrap();
            let _ = rsock.set_nodelay(true);
            let rresp = quic_tools::stream_client::exchange(&mut rsock, &[&rhdr])
                .await
                .unwrap();
            let r_us = t1.elapsed().as_micros() as u64;
            assert_eq!(split_flash_stream_response(&rresp).1, payload_ref.as_slice());
            rows_out.push(PerfRow {
                transport: "1. Plain TCP",
                bearer: "TCP",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: TransferStats {
                    elapsed_us: r_us,
                    tx_packets: 0,
                    rx_packets: 0,
                },
            });
            stop_plain.store(true, Ordering::Release);
            let _ = plain_pump.await;
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(60)) => panic!("Plain TCP perf timed out"),
                    _ = client => {},
                }
            })
            .await;
        rows.extend(tcp_rows);
        rtest.stop();
        flash.stop();
        rt.run(0);
    }

    // ------------------------------------------------------------------------
    // 3. `quic-mux` only (`mod_mux` `52` over TCP `--stream 4` & PPP `245`)
    // ------------------------------------------------------------------------
    {
        let (rt, rtest, flash, tmod) =
            attach_flash_and_load_modules(&flash_path, false, true, false, Some("mod_mux"));
        let mux_mod = tmod.unwrap();
        let tcp_addr = mux_mod
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let rt_serial = rt.clone();
        let payload_ref = &payload_1k;
        let mut mux_rows = Vec::new();
        let rows_out = &mut mux_rows;
        let client = async move {
            // (a) TCP `--stream 4`
            let stop_tcp = Arc::new(AtomicBool::new(false));
            let stop_tcp_bg = stop_tcp.clone();
            let rt_tcp_pump = rt_serial.clone();
            let tcp_pump = tokio::task::spawn_local(async move {
                while !stop_tcp_bg.load(Ordering::Acquire) {
                    rt_tcp_pump.run(5);
                    tokio::task::yield_now().await;
                }
            });

            let target = MuxClientTarget {
                tcp: Some(tcp_addr),
                ..Default::default()
            };
            let (cli_mux, _acc, _guard) = connect_mux_client(&target, TokioMuxConfig::default())
                .await
                .unwrap();

            let mut req_id = 3000u64;
            for (sz, up_label, down_label) in sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let t0 = Instant::now();
                    let res = mux_probe(&cli_mux, req, req_id).await.unwrap();
                    let elapsed_us = t0.elapsed().as_micros() as u64;
                    assert_eq!(res.bytes, sz);
                    rows_out.push(PerfRow {
                        transport: "3. mod_mux (quic-mux)",
                        bearer: "TCP",
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

            let mut wreq = encode_flash_write_header(b"p_mx_t", payload_ref.len(), 3090);
            wreq.extend_from_slice(payload_ref);
            let t0 = Instant::now();
            let wresp = mux_exchange(&cli_mux, &wreq).await.unwrap();
            let w_us = t0.elapsed().as_micros() as u64;
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
                Some(1)
            );
            rows_out.push(PerfRow {
                transport: "3. mod_mux (quic-mux)",
                bearer: "TCP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: TransferStats {
                    elapsed_us: w_us,
                    tx_packets: 0,
                    rx_packets: 0,
                },
            });

            let t1 = Instant::now();
            let rresp = mux_exchange(&cli_mux, &encode_flash_read_header(b"p_mx_t", 3091))
                .await
                .unwrap();
            let r_us = t1.elapsed().as_micros() as u64;
            assert_eq!(split_flash_stream_response(&rresp).1, payload_ref.as_slice());
            rows_out.push(PerfRow {
                transport: "3. mod_mux (quic-mux)",
                bearer: "TCP",
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
            stop_tcp.store(true, Ordering::Release);
            let _ = tcp_pump.await;

            // (b) PPP serial `--port 245`
            let (ser_mux, _ser_acc) = TokioMux::spawn(true, TokioMuxConfig::default());
            let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
            let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
            ser_mux.add_packet_link(out_tx, in_rx, PPP_MUX_MTU, 20);

            let stop = Arc::new(AtomicBool::new(false));
            let stop_bg = stop.clone();
            let tx_cnt = Arc::new(AtomicU64::new(0));
            let rx_cnt = Arc::new(AtomicU64::new(0));
            let tx_bg = tx_cnt.clone();
            let rx_bg = rx_cnt.clone();
            let rt_pump = rt_serial.clone();
            let bridge = tokio::task::spawn_local(async move {
                while !stop_bg.load(Ordering::Acquire) {
                    let mut moved = false;
                    while let Ok(frame) = out_rx.try_recv() {
                        assert_eq!(rt_pump.inject_frame("serial", 245, 0, &frame), 0);
                        tx_bg.fetch_add(1, Ordering::Relaxed);
                        rt_pump.run(0);
                        moved = true;
                    }
                    for (_tok, reply) in rt_pump.take_link_tx("serial") {
                        rx_bg.fetch_add(1, Ordering::Relaxed);
                        if in_tx.send(reply).await.is_err() {
                            return;
                        }
                        moved = true;
                    }
                    if !moved {
                        rt_pump.run(5);
                        tokio::task::yield_now().await;
                    }
                }
            });

            for (sz, up_label, down_label) in sizes {
                for (dir, label) in [
                    (ProbeDirection::Upload, up_label),
                    (ProbeDirection::Download, down_label),
                ] {
                    req_id += 1;
                    let mut req = ProbeServiceRequest::new(sz, 1024);
                    req.direction = dir;
                    req.verify = true;
                    let tx0 = tx_cnt.load(Ordering::Relaxed);
                    let rx0 = rx_cnt.load(Ordering::Relaxed);
                    let t0 = Instant::now();
                    let res = mux_probe(&ser_mux, req, req_id).await.unwrap();
                    let elapsed_us = t0.elapsed().as_micros() as u64;
                    assert_eq!(res.bytes, sz);
                    rows_out.push(PerfRow {
                        transport: "3. mod_mux (quic-mux)",
                        bearer: "PPP (245)",
                        operation: label,
                        bytes: sz,
                        stats: TransferStats {
                            elapsed_us,
                            tx_packets: tx_cnt.load(Ordering::Relaxed) - tx0,
                            rx_packets: rx_cnt.load(Ordering::Relaxed) - rx0,
                        },
                    });
                }
            }

            let mut wreq_s = encode_flash_write_header(b"p_mx_s", payload_ref.len(), 3190);
            wreq_s.extend_from_slice(payload_ref);
            let tx0 = tx_cnt.load(Ordering::Relaxed);
            let rx0 = rx_cnt.load(Ordering::Relaxed);
            let t0 = Instant::now();
            let wresp_s = mux_exchange(&ser_mux, &wreq_s).await.unwrap();
            let w_us = t0.elapsed().as_micros() as u64;
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp_s).0, 1).unwrap(),
                Some(1)
            );
            rows_out.push(PerfRow {
                transport: "3. mod_mux (quic-mux)",
                bearer: "PPP (245)",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: TransferStats {
                    elapsed_us: w_us,
                    tx_packets: tx_cnt.load(Ordering::Relaxed) - tx0,
                    rx_packets: rx_cnt.load(Ordering::Relaxed) - rx0,
                },
            });

            let tx1 = tx_cnt.load(Ordering::Relaxed);
            let rx1 = rx_cnt.load(Ordering::Relaxed);
            let t1 = Instant::now();
            let rresp_s = mux_exchange(&ser_mux, &encode_flash_read_header(b"p_mx_s", 3191))
                .await
                .unwrap();
            let r_us = t1.elapsed().as_micros() as u64;
            assert_eq!(split_flash_stream_response(&rresp_s).1, payload_ref.as_slice());
            rows_out.push(PerfRow {
                transport: "3. mod_mux (quic-mux)",
                bearer: "PPP (245)",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: TransferStats {
                    elapsed_us: r_us,
                    tx_packets: tx_cnt.load(Ordering::Relaxed) - tx1,
                    rx_packets: rx_cnt.load(Ordering::Relaxed) - rx1,
                },
            });

            ser_mux.close();
            stop.store(true, Ordering::Release);
            let _ = bridge.await;
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(60)) => panic!("mod_mux perf timed out"),
                    _ = client => {},
                }
            })
            .await;
        rows.extend(mux_rows);
        mux_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
    }

    // ------------------------------------------------------------------------
    // 4. `quic-link` standalone (`mod_qlink` `54` over UDP & PPP `242`)
    // ------------------------------------------------------------------------
    for (link_name, bearer_label, token, mtu, is_serial) in [
        ("udp", "UDP", 7u64, 1472usize, false),
        ("serial", "PPP (242)", 242u64, 1500usize, true),
    ] {
        let (rt, rtest, flash, tmod) = attach_flash_and_load_modules(
            &flash_path,
            !is_serial,
            is_serial,
            false,
            Some("mod_qlink"),
        );
        let qlink_mod = tmod.unwrap();
        let mut cli = QlinkHostClient::new(link_name, token, mtu);
        let mut req_id = 4000u64;

        for (sz, up_label, down_label) in sizes {
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
                let upload =
                    (dir == ProbeDirection::Upload).then(|| ProbePayload::new(sz, 1024).unwrap());
                let mut reader = ProbeResponseReader::for_request(req);
                let (_, stats) =
                    cli.execute_stream(&rt, &hdr[..n], upload, &[], Some(&mut reader));
                assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                rows.push(PerfRow {
                    transport: "4. mod_qlink (quic-link)",
                    bearer: bearer_label,
                    operation: label,
                    bytes: sz,
                    stats,
                });
            }
        }

        let fname = if is_serial { b"p_qk_s".as_slice() } else { b"p_qk_u".as_slice() };
        let whdr = encode_flash_write_header(fname, payload_1k.len(), 4090);
        let (wresp, wstats) = cli.execute_stream(&rt, &whdr, None, &payload_1k, None);
        assert_eq!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
            Some(1)
        );
        rows.push(PerfRow {
            transport: "4. mod_qlink (quic-link)",
            bearer: bearer_label,
            operation: "mod_flash write (1 KiB)",
            bytes: 1024,
            stats: wstats,
        });

        let (rresp, rstats) =
            cli.execute_stream(&rt, &encode_flash_read_header(fname, 4091), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
        rows.push(PerfRow {
            transport: "4. mod_qlink (quic-link)",
            bearer: bearer_label,
            operation: "mod_flash read (1 KiB)",
            bytes: 1024,
            stats: rstats,
        });

        qlink_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
    }

    // ------------------------------------------------------------------------
    // 5. `quic-link` + `quic-mux` (`mod_quic2` `55` over UDP, PPP `243`, & TCP `--stream 6`)
    // ------------------------------------------------------------------------
    for (link_name, bearer_label, token, mtu, is_serial) in [
        ("udp", "UDP", 9u64, 1472usize, false),
        ("serial", "PPP (243)", 243u64, 1500usize, true),
    ] {
        let (rt, rtest, flash, tmod) = attach_flash_and_load_modules(
            &flash_path,
            !is_serial,
            is_serial,
            false,
            Some("mod_quic2"),
        );
        let quic2_mod = tmod.unwrap();
        let mut cli = Quic2LinkHostClient::new(link_name, token, mtu);
        let mut req_id = 5000u64;

        for (sz, up_label, down_label) in sizes {
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
                let upload =
                    (dir == ProbeDirection::Upload).then(|| ProbePayload::new(sz, 1024).unwrap());
                let mut reader = ProbeResponseReader::for_request(req);
                let (_, stats) =
                    cli.execute_stream(&rt, &hdr[..n], upload, &[], Some(&mut reader));
                assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                rows.push(PerfRow {
                    transport: "5. mod_quic2 (link+mux)",
                    bearer: bearer_label,
                    operation: label,
                    bytes: sz,
                    stats,
                });
            }
        }

        let fname = if is_serial { b"p_q2_s".as_slice() } else { b"p_q2_u".as_slice() };
        let whdr = encode_flash_write_header(fname, payload_1k.len(), 5090);
        let (wresp, wstats) = cli.execute_stream(&rt, &whdr, None, &payload_1k, None);
        assert_eq!(
            cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
            Some(1)
        );
        rows.push(PerfRow {
            transport: "5. mod_quic2 (link+mux)",
            bearer: bearer_label,
            operation: "mod_flash write (1 KiB)",
            bytes: 1024,
            stats: wstats,
        });

        let (rresp, rstats) =
            cli.execute_stream(&rt, &encode_flash_read_header(fname, 5091), None, &[], None);
        assert_eq!(split_flash_stream_response(&rresp).1, payload_1k.as_slice());
        rows.push(PerfRow {
            transport: "5. mod_quic2 (link+mux)",
            bearer: bearer_label,
            operation: "mod_flash read (1 KiB)",
            bytes: 1024,
            stats: rstats,
        });

        quic2_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
    }

    // `mod_quic2` over TCP `--stream 6`
    {
        let (rt, rtest, flash, tmod) =
            attach_flash_and_load_modules(&flash_path, false, false, false, Some("mod_quic2"));
        let quic2_mod = tmod.unwrap();
        let tcp_addr = quic2_mod
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap()
            .stream
            .unwrap();

        let payload_ref = &payload_1k;
        let mut q2_tcp_rows = Vec::new();
        let rows_out = &mut q2_tcp_rows;
        let rt_tcp = rt.clone();
        let client = async move {
            let mut tcp_cli = Quic2TcpHostClient::connect(tcp_addr).await;
            let mut req_id = 5500u64;
            for (sz, up_label, down_label) in sizes {
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
                        .execute_stream(&rt_tcp, &hdr[..n], upload, &[], Some(&mut reader))
                        .await;
                    assert_eq!(reader.bytes() + reader.uploaded_bytes(), sz);
                    rows_out.push(PerfRow {
                        transport: "5. mod_quic2 (link+mux)",
                        bearer: "TCP",
                        operation: label,
                        bytes: sz,
                        stats,
                    });
                }
            }

            let whdr = encode_flash_write_header(b"p_q2_t", payload_ref.len(), 5590);
            let (wresp, wstats) = tcp_cli
                .execute_stream(&rt_tcp, &whdr, None, payload_ref, None)
                .await;
            assert_eq!(
                cbor::get_int(split_flash_stream_response(&wresp).0, 1).unwrap(),
                Some(1)
            );
            rows_out.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "TCP",
                operation: "mod_flash write (1 KiB)",
                bytes: 1024,
                stats: wstats,
            });

            let (rresp, rstats) = tcp_cli
                .execute_stream(&rt_tcp, &encode_flash_read_header(b"p_q2_t", 5591), None, &[], None)
                .await;
            assert_eq!(split_flash_stream_response(&rresp).1, payload_ref.as_slice());
            rows_out.push(PerfRow {
                transport: "5. mod_quic2 (link+mux)",
                bearer: "TCP",
                operation: "mod_flash read (1 KiB)",
                bytes: 1024,
                stats: rstats,
            });
            tcp_cli.close().await;
        };

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::select! {
                    _ = rt.run_realtime(Duration::from_secs(60)) => panic!("mod_quic2 TCP perf timed out"),
                    _ = client => {},
                }
            })
            .await;
        rows.extend(q2_tcp_rows);
        rt.run(0);
        quic2_mod.stop();
        rtest.stop();
        flash.stop();
        rt.run(0);
    }

    let _ = std::fs::remove_file(&flash_path);

    println!("\n=== HOST RUNTIME TRANSPORT PERFORMANCE BENCHMARKS ===");
    println!(
        "| {:<24} | {:<10} | {:<28} | {:>9} | {:>10} | {:>10} | {:>10} | {:>8} | {:>8} |",
        "Transport", "Bearer", "Operation", "Bytes", "Time (us)", "MiB/s", "Mbps", "TX Pkts", "RX Pkts"
    );
    println!(
        "|{:-<26}|{:-<12}|{:-<30}|{:->11}|{:->12}|{:->12}|{:->12}|{:->10}|{:->10}|",
        "", "", "", "", "", "", "", "", ""
    );
    for r in &rows {
        println!(
            "| {:<24} | {:<10} | {:<28} | {:>9} | {:>10} | {:>10.2} | {:>10.2} | {:>8} | {:>8} |",
            r.transport,
            r.bearer,
            r.operation,
            r.bytes,
            r.stats.elapsed_us,
            r.stats.mib_per_sec(r.bytes as usize),
            r.stats.mbps(r.bytes as usize),
            r.stats.tx_packets,
            r.stats.rx_packets
        );
    }
    println!("=====================================================\n");
}
