//! Opt-in hardware relay exercise. All physical identities come from the
//! selected E2E matrix, never from this executable. Run server then client.
use anyhow::{Context, Result, bail, ensure};
use quic_lite::{AssociationLimits, BearerName, PacketMeta, PeerL2Address, QuicNode, RelayHop};
use quic_lite::{bearer_udp::TokioUdpBearer, packet_pool::PacketPool, tokio::TokioNodeDriver};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

type Pool = PacketPool<32, { quic_lite::DEFAULT_PACKET_POOL_SLOT_SIZE }>;
static POOL: Pool = Pool::new();

#[derive(Deserialize)]
struct Matrix {
    devices: Vec<Device>,
    pairs: Vec<Pair>,
}
#[derive(Deserialize)]
struct Device {
    name: String,
    kind: String,
    serial: Option<String>,
    udp: Option<SocketAddr>,
    #[serde(default = "enabled")]
    e2e_enabled: bool,
}
fn enabled() -> bool {
    true
}
#[derive(Deserialize)]
struct Pair {
    #[serde(default)]
    verify: bool,
    name: String,
    source: String,
    target: String,
    tests: Vec<String>,
    bytes: usize,
    #[serde(default = "probe_direction")]
    direction: String,
    #[serde(default = "timeout")]
    timeout_secs: u64,
}
fn timeout() -> u64 {
    60
}
fn probe_direction() -> String {
    "both".to_owned()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: companion_relay server|client|usb|sta MATRIX_PAIR"
    );
    let path = std::env::var("DMESH_E2E_CONFIG").context("DMESH_E2E_CONFIG required")?;
    let matrix: Matrix = toml::from_str(&std::fs::read_to_string(path)?)?;
    let pair = matrix
        .pairs
        .iter()
        .find(|p| p.name == args[2])
        .context("pair absent")?;
    let mut probe = dmesh_server::probe::ProbeServiceRequest::new(pair.bytes as u64, 1024);
    probe.verify = pair.verify;
    probe.direction = match pair.direction.as_str() {
        "download" => dmesh_server::probe::ProbeDirection::Download,
        "upload" => dmesh_server::probe::ProbeDirection::Upload,
        "both" => dmesh_server::probe::ProbeDirection::Both,
        _ => bail!("invalid probe direction"),
    };
    ensure!(
        pair.tests.iter().any(|t| t == "usb-sta-relay"),
        "pair not enabled for this test"
    );
    let host = matrix
        .devices
        .iter()
        .find(|d| d.name == pair.source)
        .context("host absent")?;
    let companion = matrix
        .devices
        .iter()
        .find(|d| d.name == pair.target)
        .context("companion absent")?;
    ensure!(
        host.kind == "host" && companion.kind == "esp" && host.e2e_enabled && companion.e2e_enabled,
        "selected devices must be enabled host/ESP descriptors"
    );
    let host_address = host.udp.context("host UDP endpoint required")?;
    let companion_address = companion.udp.context("companion UDP endpoint required")?;
    let SocketAddr::V6(host_v6) = host_address else {
        bail!("IPv6 host required")
    };
    ensure!(
        pair.bytes > 0 && pair.bytes <= 16 * 1024 * 1024,
        "invalid test size"
    );
    let mut node = QuicNode::<Pool>::new(&POOL);
    let limits = AssociationLimits::host();
    node.set_default_association_limits(limits)
        .map_err(|e| anyhow::anyhow!("limits {e:?}"))?;
    let server = args[1] == "server";
    let direct_sta = args[1] == "sta";
    let relayed = args[1] == "client";
    ensure!(
        server || relayed || direct_sta || args[1] == "usb",
        "unknown mode"
    );
    let (bearer, peer) = if server || direct_sta {
        let bind = if server {
            host_address
        } else {
            quic_lite::bearer_udp::wildcard_bind(companion_address, 0)
        };
        let udp = TokioUdpBearer::<32>::bind(bind).await?;
        let peer = udp
            .register_peer(companion_address)
            .map_err(|e| anyhow::anyhow!("UDP peer {e:?}"))?;
        (
            node.add_bearer(udp)
                .map_err(|e| anyhow::anyhow!("bearer {e:?}"))?,
            peer,
        )
    } else {
        let uart = uart_codec::host::UartPort::open(
            companion.serial.as_ref().context("USB path required")?,
            None,
        )?
        .into_tokio::<32, { quic_lite::DEFAULT_PACKET_POOL_SLOT_SIZE }>(
            BearerName::new("usb").unwrap(),
        )?;
        (
            node.add_bearer(uart)
                .map_err(|e| anyhow::anyhow!("bearer {e:?}"))?,
            PeerL2Address::new(1).unwrap(),
        )
    };
    let (node, driver) = TokioNodeDriver::new(node, limits.connection);
    let scenario = async {
        let meta = PacketMeta {
            bearer,
            peer_l2_address: peer,
            received_at_us: 0,
        };
        if server {
            // Seed the observed IP/port/MAC tuple before asking the ESP to
            // route to it. No extra application protocol or address guessing.
            let seed = node
                .associate(meta, 0)
                .await
                .map_err(|e| anyhow::anyhow!("seed {e:?}"))?;
            seed.wait_established()
                .await
                .map_err(|e| anyhow::anyhow!("seed handshake {e:?}"))?;
            println!("ready=true mode=server");
            let mut stream = node
                .accept_stream()
                .await
                .context("incoming stream closed")?;
            let registry = std::sync::Arc::new(dmesh_server::registry::Registry::new());
            ensure!(
                registry.register_stream(
                    dmesh_server::registry::methods::PROBE_HANDLER,
                    dmesh_server::probe::ProbeHandler
                ),
                "probe registration"
            );
            let service = dmesh_server::stream_service::RegistryStreamHandler::new(registry);
            if probe.direction == dmesh_server::probe::ProbeDirection::Both {
                let mut download = node
                    .accept_stream()
                    .await
                    .context("download stream closed")?;
                tokio::try_join!(service.serve(&mut stream), service.serve(&mut download))?;
            } else {
                service.serve(&mut stream).await?;
            }
            println!("mode=server handler=probe.handler response_fin=true");
            match seed.finish().await {
                Ok(()) => {}
                Err(quic_lite::QuicNodeEgressError::Association(
                    quic_lite::QuicNodeError::MissingAssociation,
                )) => {
                    // Seeding only learns the companion's return tuple.
                    // Its otherwise idle association can expire during a
                    // long probe on the separate relayed association. Do not
                    // stop that association's driver before response FIN is
                    // delivered because unrelated cleanup is already done.
                    println!("mode=server seed_already_retired=true");
                }
                Err(error) => bail!("finish seed association: {error:?}"),
            }
            // Give the client time to acknowledge the response FIN.
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            let mut address = [0; 18];
            address[..16].copy_from_slice(&host_v6.ip().octets());
            address[16..].copy_from_slice(&host_v6.port().to_be_bytes());
            let hop = RelayHop::new(
                dmesh_server::transport_path::TransportId::UDP6.0,
                &address,
                120_000_000,
                1_000_000,
            )
            .map_err(|e| anyhow::anyhow!("hop {e:?}"))?;
            let started = Instant::now();
            let association = if relayed {
                node.associate_relayed(meta, None, vec![hop], 0).await
            } else {
                node.associate(meta, 0).await
            }
            .map_err(|e| anyhow::anyhow!("association {e:?}"))?;
            association
                .wait_established()
                .await
                .map_err(|e| anyhow::anyhow!("relay handshake {e:?}"))?;
            let setup_ms = started.elapsed().as_millis();
            let mut header = [0; dmesh_server::probe::PROBE_REQUEST_MAX];
            let len = dmesh_server::probe::encode_probe_handler_request(probe, 1, &mut header)
                .context("probe header")?;
            let transfer =
                dmesh_server::stream_client::probe_quic(&association, &header[..len], probe);
            tokio::pin!(transfer);
            let report_circuits = std::env::var_os("DMESH_PROBE_CIRCUIT_STATS").is_some();
            let mut reports = tokio::time::interval(Duration::from_secs(5));
            reports.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            reports.tick().await;
            let result = loop {
                tokio::select! {
                    result = &mut transfer => break result?,
                    _ = reports.tick(), if report_circuits => {
                        for circuit in association.circuits().await
                            .map_err(|error| anyhow::anyhow!("probe circuits: {error:?}"))? {
                            eprintln!(
                                "probe_circuit elapsed_ms={} srtt_us={} min_rtt_us={} cwnd_bytes={} bytes_in_flight={} sent={} acked={} lost={} spurious={} retransmitted={} ack_ranges_dropped={} last_ack_us={}",
                                started.elapsed().as_millis(), circuit.srtt_us,
                                circuit.min_rtt_us, circuit.cwnd_bytes,
                                circuit.bytes_in_flight, circuit.packets_sent,
                                circuit.packets_acked, circuit.packets_lost,
                                circuit.spurious_losses, circuit.retransmitted_packets,
                                circuit.ack_ranges_dropped, circuit.last_ack_us,
                            );
                        }
                    }
                }
            };
            ensure!(result.bytes == probe.total_bytes(), "probe length");
            association
                .finish()
                .await
                .map_err(|error| anyhow::anyhow!("finish probe association: {error:?}"))?;
            println!(
                "mode={} handler=probe.handler direction={} bytes={} verified={} fin=true setup_ms={} elapsed_us={} mbps={:.3}",
                args[1],
                probe.direction.as_str(),
                result.bytes,
                probe.verify,
                setup_ms,
                result.elapsed_us,
                result.bits_per_second() as f64 / 1_000_000.0
            );
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(pair.timeout_secs), async {
        tokio::select! {
            result = scenario => result,
            result = driver.run() => bail!("driver stopped: {result:?}"),
        }
    })
    .await
    .context("hardware relay deadline")??;
    Ok(())
}
