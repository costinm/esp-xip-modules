//! Small host adapter around quic-lite's node-owned stream API.

use std::{future::Future, net::SocketAddr, time::Duration};

use quic_lite::{
    BearerName, PacketMeta, PeerL2Address, QuicNode, QuicNodeEgressError,
    packet_pool::PacketPool,
    tokio::{TokioAssociation, TokioNodeDriver},
};

const POOL_SLOTS: usize = 16;
const SLOT_SIZE: usize = quic_lite::DEFAULT_PACKET_POOL_SLOT_SIZE;
pub(crate) type Pool = PacketPool<POOL_SLOTS, SLOT_SIZE>;
pub(crate) type Node = QuicNode<Pool>;

static CLIENT_POOL: Pool = Pool::new();
static UDP_POOL: Pool = Pool::new();

pub(crate) struct Response {
    pub bytes: Vec<u8>,
}

fn new_node() -> Node {
    let mut node = Node::new(&CLIENT_POOL);
    configure_node(&mut node);
    node
}

fn configure_node(node: &mut Node) {
    node.set_limits(quic_lite::NodeLimits::host())
        .expect("host QUIC node limits are valid");
    node.set_default_association_limits(quic_lite::AssociationLimits::host())
        .expect("host QUIC association limits are valid");
}

async fn run_node<T, F, Fut>(node: Node, operation: F) -> Result<T, String>
where
    F: FnOnce(quic_lite::tokio::TokioNode) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let (client, driver) =
        TokioNodeDriver::new(node, quic_lite::AssociationLimits::host().connection);
    let observer = client.clone();
    let operation = async move {
        let result = operation(client).await?;
        // finish() retires the association, but an accepted async bearer still
        // owns its CLOSE packet. Keep the driver alive until those physical
        // submissions complete instead of canceling them at operation return.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (_, bearers) = observer.rows().await.ok_or("QUIC driver stopped while draining sends")?;
                if bearers.iter().all(|bearer| bearer.in_flight_packets == 0) {
                    return Ok::<(), &str>(());
                }
                tokio::task::yield_now().await;
            }
        }).await.map_err(|_| "physical send completion timed out")??;
        Ok(result)
    };
    tokio::select! {
        result = driver.run() => Err(format!("QUIC node driver stopped: {result:?}")),
        result = operation => result,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClientPath {
    pub(crate) bearer: quic_lite::BearerId,
    pub(crate) peer_l2_address: PeerL2Address,
}

async fn associate_paths(
    client: &quic_lite::tokio::TokioNode,
    primary: ClientPath,
    additional: &[ClientPath],
) -> Result<TokioAssociation, String> {
    let association = client
        .associate(
            PacketMeta {
                bearer: primary.bearer,
                peer_l2_address: primary.peer_l2_address,
                received_at_us: 0,
            },
            0,
        )
        .await
        .map_err(node_error("start QUIC association"))?;
    association
        .wait_established()
        .await
        .map_err(node_error("establish QUIC association"))?;
    for path in additional {
        let circuit = association
            .add_direct_circuit(
                quic_lite::CircuitNextHop {
                    bearer: path.bearer,
                    peer_l2_address: path.peer_l2_address,
                },
                0,
            )
            .await
            .map_err(node_error("join additional QUIC bearer"))?;
        eprintln!("dmesh_cli_path_join queued circuit={circuit:?}");
    }
    Ok(association)
}

fn attach_uart(node: &mut Node, path: &str, baud: Option<u32>) -> Result<ClientPath, String> {
    let port = uart_codec::host::UartPort::open(path, baud).map_err(|error| error.to_string())?;
    let mut bearer_impl = port
        .into_tokio::<POOL_SLOTS, SLOT_SIZE>(BearerName::new("uart").unwrap())
        .map_err(|error| error.to_string())?;
    bearer_impl.set_sideband_handler(|received| {
        for line in received.logs {
            eprintln!("dmesh_uart_console {line:?}");
        }
        for bytes in received.log_records {
            // Native NAN reports are a separate PPP channel, consumed by an
            // explicit watch. They are not malformed CBOR or stream diagnostics.
            if bytes.first()==Some(&(crate::xip_wire::PORT_NAN_REPORT as u8)){continue;}
            eprintln!("{}", uart_codec::host::render_sideband_record(&bytes));
        }
        if received.pool_drops != 0 {
            eprintln!("dmesh_uart_log_drop pool={}", received.pool_drops);
        }
    });
    let bearer = node
        .add_bearer(bearer_impl)
        .map_err(|error| format!("attach UART bearer: {error:?}"))?;
    Ok(ClientPath {
        bearer,
        peer_l2_address: PeerL2Address::new(1).unwrap(),
    })
}

async fn attach_udp(
    node: &mut Node,
    bind: SocketAddr,
    peer: SocketAddr,
) -> Result<ClientPath, String> {
    // The fixed client port can be busy (a second CLI running): then any free port will do.
    let bearer_impl = match quic_lite::bearer_udp::TokioUdpBearer::<8>::bind(bind).await {
        Ok(bearer) => bearer,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && bind.port() != 0 => {
            let mut any = bind;
            any.set_port(0);
            quic_lite::bearer_udp::TokioUdpBearer::<8>::bind(any)
                .await
                .map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    let peer_l2_address = bearer_impl
        .register_peer(peer)
        .map_err(|error| format!("register UDP peer: {error:?}"))?;
    let bearer = node
        .add_bearer(bearer_impl)
        .map_err(|error| format!("attach UDP bearer: {error:?}"))?;
    Ok(ClientPath {
        bearer,
        peer_l2_address,
    })
}

async fn exchange(association: TokioAssociation, payload: &[u8]) -> Result<Response, String> {
    association
        .wait_established()
        .await
        .map_err(node_error("establish association"))?;
    let mut request = association
        .open_stream()
        .await
        .map_err(node_error("open request stream"))?;
    let bytes = dmesh_server::stream_client::exchange(&mut request, &[payload])
        .await
        .map_err(|error| format!("request stream exchange: {error}"))?;
    association
        .finish()
        .await
        .map_err(node_error("finish association"))?;
    Ok(Response { bytes })
}

fn node_error(context: &'static str) -> impl FnOnce(QuicNodeEgressError) -> String {
    move |error| format!("{context}: {error:?}")
}

pub(crate) async fn request_udp(
    bind: SocketAddr,
    peer: SocketAddr,
    payload: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let future = async {
        let mut node = Node::new(&UDP_POOL);
        configure_node(&mut node);
        let primary = attach_udp(&mut node, bind, peer).await?;
        run_node(node, |client| async move {
            let association = associate_paths(&client, primary, &[]).await?;
            exchange(association, payload).await
        })
        .await
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("UDP stream request timed out after {timeout:?}"))?
}

pub(crate) async fn request_udp_with_uart(
    bind: SocketAddr,
    peer: SocketAddr,
    uart_path: Option<&str>,
    uart_baud: Option<u32>,
    payload: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let future = async {
        let mut node = new_node();
        let primary = attach_udp(&mut node, bind, peer).await?;
        let additional = uart_path
            .map(|path| attach_uart(&mut node, path, uart_baud))
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        let (client, driver) =
            TokioNodeDriver::new(node, quic_lite::AssociationLimits::host().connection);
        let operation = async move {
            let association = associate_paths(&client, primary, &additional).await?;
            exchange(association, payload).await
        };
        tokio::select! {
            result = driver.run() => Err(format!("UDP QUIC driver stopped: {result:?}")),
            result = operation => result,
        }
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("UDP stream request timed out after {timeout:?}"))?
}

/// Run the binary QUIC probe workload over one UDP association. The request is
/// still the canonical tagged service record; the response body is validated
/// incrementally so large probes do not need to be retained in one Vec.
#[cfg(test)]
pub(crate) async fn probe_udp(
    bind: SocketAddr,
    peer: SocketAddr,
    payload: &[u8],
    request: dmesh_server::probe::ProbeServiceRequest,
    timeout: Duration,
) -> Result<dmesh_server::stream_client::ProbeResult, String> {
    probe_udp_with_uart(bind, peer, None, None, payload, request, timeout).await
}

pub(crate) async fn probe_udp_with_uart(
    bind: SocketAddr,
    peer: SocketAddr,
    uart_path: Option<&str>,
    uart_baud: Option<u32>,
    payload: &[u8],
    request: dmesh_server::probe::ProbeServiceRequest,
    timeout: Duration,
) -> Result<dmesh_server::stream_client::ProbeResult, String> {
    let future = async {
        let mut node = new_node();
        let primary = attach_udp(&mut node, bind, peer).await?;
        let additional = uart_path
            .map(|path| attach_uart(&mut node, path, uart_baud))
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        run_node(node, |client| async move {
            let association = associate_paths(&client, primary, &additional).await?;
            exchange_probe(association, payload, request, None).await
        })
        .await
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("UDP QUIC probe timed out after {timeout:?}"))?
}

/// Run the validated streaming probe over the host UART bearer. The result is
/// consumed in packet-sized chunks, so a large workload does not become a
/// correspondingly large host allocation.
pub(crate) async fn probe_uart(
    path: &str,
    baud: Option<u32>,
    payload: &[u8],
    request: dmesh_server::probe::ProbeServiceRequest,
    timeout: Duration,
    linger: Duration,
) -> Result<dmesh_server::stream_client::ProbeResult, String> {
    let mut node = new_node();
    let primary = attach_uart(&mut node, path, baud)?;
    run_node(node, |client| async move {
        let attempt = async {
            let association = associate_paths(&client, primary, &[]).await?;
            exchange_probe(association, payload, request, None).await
        };
        monitor_uart_probe(attempt, timeout, linger).await
    })
    .await
}

/// The caller keeps the UART driver alive while this operation is pending.
/// Log collection follows both completion and timeout, outside the deadline.
async fn monitor_uart_probe<T>(
    attempt: impl core::future::Future<Output = Result<T, String>>,
    timeout: Duration,
    linger: Duration,
) -> Result<T, String> {
    let result = tokio::time::timeout(timeout, attempt)
        .await
        .map_err(|_| format!("UART QUIC probe timed out after {timeout:?}"))
        .and_then(|result| result);
    tokio::time::sleep(linger).await;
    result
}

/// Run a probe as the client of the peer's `probe.handler` handler: send the
/// request header (no body, FIN right after it), then validate the response:
/// a header, then the probe payload, however the stream is chunked.
async fn exchange_probe(
    association: TokioAssociation,
    payload: &[u8],
    request: dmesh_server::probe::ProbeServiceRequest,
    stats: Option<&quic_lite::tokio::TokioNode>,
) -> Result<dmesh_server::stream_client::ProbeResult, String> {
    let result = dmesh_server::stream_client::probe_quic(&association, payload, request)
        .await
        .map_err(|error| format!("probe stream exchange: {error}"))?;
    if let Some(client) = stats {
        let t = client.telemetry().await.ok_or("QUIC telemetry unavailable")?;
        println!("{}", serde_json::json!({"kind":"quic.stats","side":"host",
            "live_associations":t.live_associations,"received_packets":t.received_packets,
            "sent_packets":t.sent_packets,"duplicate_packets":t.duplicate_packets,
            "out_of_order_packets":t.out_of_order_packets,"inferred_missing_packets":t.inferred_missing_packets,
            "retransmitted_packets":t.retransmitted_packets,"loss_events":t.loss_events,
            "submission_failures":t.submission_failures,"received_stream_packets":t.received_stream_packets,
            "received_control_packets":t.received_control_packets,"sent_stream_packets":t.sent_stream_packets,
            "sent_control_packets":t.sent_control_packets}));
    }
    association
        .finish()
        .await
        .map_err(node_error("finish QUIC probe association"))?;
    Ok(result)
}

pub(crate) async fn request_uart(
    path: &str,
    baud: Option<u32>,
    payload: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    request_uart_monitor(
        path,
        baud,
        || Ok(payload.to_vec()),
        timeout,
        None,
        Duration::ZERO,
        |_| {},
    )
    .await
}

/// Keep one physical UART bearer and Tokio node alive while issuing repeated
/// request streams. The payload factory lets callers assign a fresh request ID
/// to each iteration. After a one-shot response, or a failed repeated request,
/// the driver remains active for `linger` so late sideband/crash logs are saved.
pub(crate) async fn request_uart_monitor<F, G>(
    path: &str,
    baud: Option<u32>,
    mut payload: F,
    timeout: Duration,
    repeat_interval: Option<Duration>,
    linger: Duration,
    mut on_response: G,
) -> Result<Response, String>
where
    F: FnMut() -> Result<Vec<u8>, String>,
    G: FnMut(&Response),
{
    let mut node = new_node();
    let primary = attach_uart(&mut node, path, baud)?;
    let future = async {
        run_node(node, |client| async move {
            let mut last_response = None;
            loop {
                let request = payload()?;
                let attempt = async {
                    let association = associate_paths(&client, primary, &[]).await?;
                    exchange(association, &request).await
                };
                let response = match tokio::time::timeout(timeout, attempt).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        tokio::time::sleep(linger).await;
                        return Err(error);
                    }
                    Err(_) => {
                        tokio::time::sleep(linger).await;
                        return Err(format!("UART stream request timed out after {timeout:?}"));
                    }
                };
                on_response(&response);
                last_response = Some(response);
                if let Some(interval) = repeat_interval {
                    tokio::time::sleep(interval).await;
                } else {
                    tokio::time::sleep(linger).await;
                    return Ok(last_response.expect("one UART request completed"));
                }
            }
        })
        .await
    };
    future.await
}

#[cfg(test)]
mod tests {
    use super::request_udp;
    use std::{
        net::{SocketAddr, UdpSocket},
        thread,
        time::Duration,
    };

    use quic_lite::{
        AssociationLimits, NodeLimits, QuicNode, packet_pool::PacketPool, tokio::TokioNodeDriver,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type TestPool = PacketPool<16, { quic_lite::DEFAULT_PACKET_POOL_SLOT_SIZE }>;
    type TestNode = QuicNode<TestPool>;

    #[tokio::test]
    async fn one_shot_client_keeps_driver_until_accepted_send_completes() {
        use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
        use quic_lite::{BearerContext, BearerInfo, BearerName, EgressSubmission,
            PacketBearer, PacketEgress, PacketMeta, PacketPool, PacketSendOutcome,
            PacketSubmitError, PeerL2Address};
        type Lease = <super::Pool as PacketPool>::Buffer;
        struct Held(Arc<Mutex<Option<EgressSubmission<Lease>>>>);
        impl PacketEgress<Lease> for Held {
            fn submit(&mut self, _: PeerL2Address, packet: EgressSubmission<Lease>) -> Result<(), PacketSubmitError<Lease>> {
                assert!(self.0.lock().unwrap().replace(packet).is_none());
                Ok(())
            }
        }
        impl PacketBearer<super::Pool> for Held {
            type AttachError = ();
            fn info(&self) -> BearerInfo {
                BearerInfo { name: BearerName::new("held").unwrap(), max_packet_size: quic_lite::DEFAULT_MAX_PACKET_SIZE,
                    prefix_required: 0, suffix_required: 0, requires_packet_encryption: false,
                    secure_link: true, nominal_bitrate_bps: 0, local_mac: None }
            }
            fn attach(&mut self, _: BearerContext<super::Pool>) -> Result<(), ()> { Ok(()) }
        }
        static POOL: super::Pool = super::Pool::new();
        let pending = Arc::new(Mutex::new(None));
        let complete = Arc::new(AtomicBool::new(false));
        let mut node = super::Node::new(&POOL);
        let bearer = node.add_bearer(Held(pending.clone())).unwrap();
        node.associate(PacketMeta { bearer, peer_l2_address: PeerL2Address::new(1).unwrap(), received_at_us: 0 }, 0).unwrap();
        assert!(pending.lock().unwrap().is_some());
        let finished = complete.clone();
        let worker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            pending.lock().unwrap().take().unwrap().complete(PacketSendOutcome::Sent, 0);
            finished.store(true, Ordering::Release);
        });
        let result = super::run_node(node, |_| async { Ok(42) }).await.unwrap();
        assert_eq!(result, 42);
        assert!(complete.load(Ordering::Acquire), "operation returned while the bearer still owned its packet");
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn uart_probe_linger_is_outside_success_deadline() {
        let start = std::time::Instant::now();
        let result = super::monitor_uart_probe(
            async { Ok::<_, String>(42) },
            Duration::from_millis(5),
            Duration::from_millis(20),
        ).await;
        assert_eq!(result.unwrap(), 42);
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn uart_probe_timeout_still_collects_logs_during_linger() {
        let start = std::time::Instant::now();
        let result = super::monitor_uart_probe(
            core::future::pending::<Result<(), String>>(),
            Duration::from_millis(5),
            Duration::from_millis(20),
        ).await;
        assert!(result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn udp_client_completes_a_quic_stream_request_and_response() {
        static SERVER_POOL: TestPool = TestPool::new();
        let request: Vec<u8> = (0..32 * 1024)
            .map(|index| b'a' + (index % 26) as u8)
            .collect();
        let expected: Vec<u8> = request.iter().map(u8::to_ascii_uppercase).collect();

        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server_socket.local_addr().unwrap();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let bearer =
                    quic_lite::bearer_udp::TokioUdpBearer::<8>::from_std(server_socket).unwrap();
                let mut node = TestNode::new(&SERVER_POOL);
                node.set_limits(NodeLimits::host()).unwrap();
                node.set_default_association_limits(AssociationLimits::host())
                    .unwrap();
                node.add_bearer(bearer).unwrap();
                let (server, driver) =
                    TokioNodeDriver::new(node, AssociationLimits::host().connection);
                let serve = async {
                    let mut stream =
                        tokio::time::timeout(Duration::from_secs(3), server.accept_stream())
                            .await
                            .expect("server did not accept the UDP QUIC stream")
                            .expect("server stream acceptance failed");
                    let mut request = Vec::new();
                    stream.read_to_end(&mut request).await.unwrap();
                    request.make_ascii_uppercase();
                    stream.write_all(&request).await.unwrap();
                    stream.shutdown().await.unwrap();
                };
                tokio::select! {
                    result = driver.run() => panic!("UDP server driver stopped: {result:?}"),
                    () = serve => {},
                }
            });
        });

        let response = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(request_udp(
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                server_address,
                &request,
                Duration::from_secs(3),
            ))
            .expect("UDP QUIC request failed");
        server.join().unwrap();
        assert_eq!(response.bytes, expected);
    }

    #[test]
    fn udp_probe_without_a_registered_handler_finishes_with_a_tagged_error() {
        static SERVER_POOL: TestPool = TestPool::new();

        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server_socket.local_addr().unwrap();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let bearer =
                    quic_lite::bearer_udp::TokioUdpBearer::<8>::from_std(server_socket).unwrap();
                let mut node = TestNode::new(&SERVER_POOL);
                node.set_limits(NodeLimits::host()).unwrap();
                node.set_default_association_limits(AssociationLimits::host())
                    .unwrap();
                node.add_bearer(bearer).unwrap();
                let (server, driver) =
                    TokioNodeDriver::new(node, AssociationLimits::host().connection);
                let serve = async {
                    let mut stream =
                        tokio::time::timeout(Duration::from_secs(3), server.accept_stream())
                            .await
                            .expect("server did not accept the UDP QUIC probe")
                            .expect("server stream acceptance failed");
                    let mut request = Vec::new();
                    stream.read_to_end(&mut request).await.unwrap();
                    let response = dmesh_server::registry::dispatch(
                        &request,
                        true,
                        dmesh_server::registry::CallContext::default(),
                    )
                    .await
                    .expect("valid unsupported probe should receive a tagged error");
                    stream.write_all(&response).await.unwrap();
                    stream.shutdown().await.unwrap();
                };
                tokio::select! {
                    result = driver.run() => panic!("UDP server driver stopped: {result:?}"),
                    () = serve => {},
                }
            });
        });

        let mut request_wire = [0u8; dmesh_server::probe::PROBE_REQUEST_MAX];
        let request_len = dmesh_server::probe::encode_probe_start_request(
            dmesh_server::probe::ProbeServiceRequest::new(4096, 512),
            0x1234,
            &mut request_wire,
        )
        .unwrap();

        let response = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(request_udp(
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                server_address,
                &request_wire[..request_len],
                Duration::from_secs(3),
            ))
            .expect("UDP QUIC probe request did not complete");
        server.join().unwrap();

        let record = dmesh_server::tagged::decode(&response.bytes).unwrap();
        assert_eq!(record.component, Some(dmesh_server::tagged::Name::Tag(8)));
        assert_eq!(record.method, Some(dmesh_server::tagged::Name::Tag(1)));
        assert_eq!(record.id, Some(0x1234));
        assert!(record.error.is_some());
    }

    #[test]
    fn udp_probe_stream_validates_the_peer_payload_and_completion() {
        static SERVER_POOL: TestPool = TestPool::new();

        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server_socket.local_addr().unwrap();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let bearer =
                    quic_lite::bearer_udp::TokioUdpBearer::<8>::from_std(server_socket).unwrap();
                let mut node = TestNode::new(&SERVER_POOL);
                node.set_limits(NodeLimits::host()).unwrap();
                node.set_default_association_limits(AssociationLimits::host())
                    .unwrap();
                node.add_bearer(bearer).unwrap();
                let (server, driver) =
                    TokioNodeDriver::new(node, AssociationLimits::host().connection);
                let serve = async {
                    for index in 0..3 {
                        let mut stream =
                            tokio::time::timeout(Duration::from_secs(3), server.accept_stream())
                                .await
                                .expect("server did not accept the UDP probe")
                                .expect("server stream acceptance failed");
                        // The probed node serves `probe.handler` as a registered
                        // handler: a header, then the payload as the body.
                        let registry = std::sync::Arc::new(dmesh_server::registry::Registry::new());
                        assert!(registry.register_stream(
                            dmesh_server::registry::methods::PROBE_HANDLER,
                            dmesh_server::probe::ProbeHandler,
                        ));
                        let service =
                            dmesh_server::stream_service::RegistryStreamHandler::new(registry);
                        if index == 2 {
                            let mut second = server.accept_stream().await.unwrap();
                            tokio::try_join!(
                                service.serve(&mut stream),
                                service.serve(&mut second)
                            )
                            .unwrap();
                        } else {
                            service.serve(&mut stream).await.unwrap();
                        }
                    }
                };
                tokio::select! {
                    result = driver.run() => panic!("UDP server driver stopped: {result:?}"),
                    () = serve => {},
                }
            });
        });

        for direction in [
            dmesh_server::probe::ProbeDirection::Download,
            dmesh_server::probe::ProbeDirection::Upload,
            dmesh_server::probe::ProbeDirection::Both,
        ] {
            let mut probe_request = dmesh_server::probe::ProbeServiceRequest::new(256 * 1024, 512);
            probe_request.direction = direction;
            let mut request_wire = [0u8; dmesh_server::probe::PROBE_REQUEST_MAX];
            let request_len = dmesh_server::probe::encode_probe_handler_request(
                probe_request,
                0x5678,
                &mut request_wire,
            )
            .unwrap();
            let result = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(super::probe_udp(
                    "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                    server_address,
                    &request_wire[..request_len],
                    probe_request,
                    Duration::from_secs(3),
                ))
                .expect("UDP probe did not finish");
            assert_eq!(result.bytes, probe_request.total_bytes());
            assert_eq!(result.normal_bytes, probe_request.total_bytes());
            assert_eq!(result.callback_errors, [0; 6]);
            assert!(result.elapsed_us > 0);
        }
        server.join().unwrap();
    }
}

/// Upload on one native module stream, optionally through an explicit circuit.
pub(crate) async fn upload(target:&str,baud:Option<u32>,package:&[u8],via:Option<crate::via::Via>,timeout:Duration)->Result<Vec<u8>,String>{
    use tokio::io::{AsyncReadExt,AsyncWriteExt};
    let operation=async {
        let mut node=new_node();
        let primary=if let Some(via)=&via {via.attach(&mut node,baud)?} else if target.starts_with("udp://") {
            let peer=crate::client::parse_udp_peer(target)?;
            attach_udp(&mut node,quic_lite::bearer_udp::wildcard_bind(peer,0),peer).await?
        }else{attach_uart(&mut node,target,baud)?};
        run_node(node,|client|async move{
            let association=associate_paths(&client,primary,&[]).await?;
            association.wait_established().await.map_err(node_error("establish upload"))?;
            let stream=association.open_stream().await.map_err(node_error("upload stream"))?;
            let(mut rx,mut tx)=tokio::io::split(stream);
            // One self-delimiting envelope followed by the unchanged WLU1 body.
            let mut e=minicbor::Encoder::new(Vec::new());
            e.map(4).unwrap().u8(1).unwrap().u8(44).unwrap().u8(2).unwrap().u8(1).unwrap().u8(3).unwrap().u64(crate::client::fresh_request_id()&i64::MAX as u64).unwrap().u8(5).unwrap().map(0).unwrap();
            let sending=async{tx.write_all(e.writer()).await?;tx.write_all(package).await?;tx.shutdown().await};
            let receiving=async{let mut bytes=Vec::new();(&mut rx).take(513).read_to_end(&mut bytes).await.map_err(|e|e.to_string())?;if bytes.len()>512{return Err("upload response exceeds 512 bytes".to_string())}Ok(bytes)};
            tokio::pin!(sending,receiving);
            let response=tokio::select!{
                reply=&mut receiving=>reply?,
                sent=&mut sending=>{let reply=receiving.await?;if reply.is_empty(){sent.map_err(|e|e.to_string())?;}reply}
            };
            association.finish().await.map_err(node_error("close upload association"))?;
            Ok(response)
        }).await
    };
    tokio::time::timeout(timeout,operation).await.map_err(|_|format!("QUIC upload timed out after {timeout:?}"))?
}

pub(crate) async fn native_probe(target:&str,baud:Option<u32>,via:Option<crate::via::Via>,request:dmesh_server::probe::ProbeServiceRequest,timeout:Duration,stats:bool)->Result<dmesh_server::stream_client::ProbeResult,String>{
    let mut header=[0;dmesh_server::probe::PROBE_REQUEST_MAX];
    let id=crate::client::fresh_request_id()&i64::MAX as u64;
    let used=dmesh_server::probe::encode_probe_handler_request(request,id,&mut header).ok_or("probe header too large")?;
    let operation=async {
        if target.starts_with("tcp://") && via.is_none() {
            let peer=crate::client::parse_udp_peer(target.trim_start_matches("tcp://"))?;
            let mut socket=tokio::net::TcpStream::connect(peer).await.map_err(|e|e.to_string())?;
            socket.set_nodelay(true).map_err(|e|e.to_string())?;
            return if request.direction==dmesh_server::probe::ProbeDirection::Both {
                let mut down=tokio::net::TcpStream::connect(peer).await.map_err(|e|e.to_string())?;
                dmesh_server::stream_client::probe_duplex(&mut socket,&mut down,request,id).await
            }else{dmesh_server::stream_client::probe(&mut socket,&header[..used],request).await}.map_err(|e|e.to_string());
        }
        let mut node=new_node();
        let primary=if let Some(via)=&via {via.attach(&mut node,baud)?} else if target.starts_with("udp://") {
            let peer=crate::client::parse_udp_peer(target)?;attach_udp(&mut node,quic_lite::bearer_udp::wildcard_bind(peer,0),peer).await?
        }else{attach_uart(&mut node,target,baud)?};
        run_node(node,|client|async move{let association=associate_paths(&client,primary,&[]).await?;exchange_probe(association,&header[..used],request,if stats {Some(&client)} else {None}).await}).await
    };
    tokio::time::timeout(timeout,operation).await.map_err(|_|format!("probe timed out after {timeout:?}"))?
}

/// Read report bodies over the same module stream interface used by flash/probes.
pub(crate) async fn nan_stream(target:&str,baud:Option<u32>,via:Option<crate::via::Via>,request:&[u8],duration:Duration)->Result<(),String>{
    if target.starts_with("tcp://") && via.is_none() {return Err("NAN has no direct TCP listener; use a QUIC bearer".into());}
    let mut node=new_node();
    let primary=if let Some(via)=&via {via.attach(&mut node,baud)?} else if target.starts_with("udp://") {let peer=crate::client::parse_udp_peer(target)?;attach_udp(&mut node,quic_lite::bearer_udp::wildcard_bind(peer,0),peer).await?}else{attach_uart(&mut node,target,baud)?};
    run_node(node,|client|async move{
        let association=associate_paths(&client,primary,&[]).await?;
        association.wait_established().await.map_err(node_error("establish NAN stream"))?;
        let mut stream=association.open_stream().await.map_err(node_error("open NAN stream"))?;
        write_nan_watch(&mut stream,request).await?;
        let result=read_nan_stream(&mut stream,duration).await;
        association.finish().await.map_err(node_error("close NAN stream"))?;
        result
    }).await
}
// The watch has no body: flush the opening request and half-close TX, retaining RX.
async fn write_nan_watch(stream:&mut (impl tokio::io::AsyncWrite+Unpin),request:&[u8])->Result<(),String>{
    use tokio::io::AsyncWriteExt;
    stream.write_all(request).await.map_err(|e|e.to_string())?;
    stream.shutdown().await.map_err(|e|e.to_string())
}
async fn read_nan_stream(stream:&mut (impl tokio::io::AsyncRead+Unpin),duration:Duration)->Result<(),String>{
    use tokio::io::AsyncReadExt;
    let deadline=tokio::time::Instant::now()+duration;
    let mut pending=Vec::new();let mut chunk=[0u8;1500];let mut records=0;
    loop {
        let n=match tokio::time::timeout_at(deadline,stream.read(&mut chunk)).await {
            Err(_)=>break,Ok(Err(e))=>return Err(e.to_string()),Ok(Ok(n))=>n,
        };
        if n==0 {if !pending.is_empty(){return Err("NAN stream ended in a partial CBOR record".into())}break}
        pending.extend_from_slice(&chunk[..n]);
        loop {
            let mut decoder=minicbor::Decoder::new(&pending);
            match decoder.skip() {
                Ok(())=>{},
                Err(e) if e.is_end_of_input()=>break,
                Err(e)=>return Err(format!("invalid NAN report CBOR: {e}")),
            }
            let n=decoder.position();
            if n==0{return Err("empty NAN record".into())}
            let record=crate::console::report(&pending[..n])?;
            println!("{record}");records+=1;pending.drain(..n);
        }
        if pending.len()>1500{return Err("NAN stream record exceeds one packet".into())}
    }
    if records==0 {Err("NAN stream returned no records".into())}
    else if !pending.is_empty(){Err("NAN stream ended in a partial CBOR record".into())}
    else{Ok(())}
}

#[cfg(test)]
mod nan_stream_tests {
    use super::*;
    #[test]
    fn watch_half_closes_only_the_sending_direction(){
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use tokio::io::{AsyncReadExt,AsyncWriteExt};
            let(client,mut peer)=tokio::io::duplex(64);
            let mut client=tokio::io::BufWriter::new(client);
            write_nan_watch(&mut client,b"open").await.unwrap();
            let mut bytes=Vec::new();peer.read_to_end(&mut bytes).await.unwrap();assert_eq!(bytes,b"open");
            peer.write_all(b"report").await.unwrap();let mut received=[0;6];
            client.read_exact(&mut received).await.unwrap();assert_eq!(&received,b"report");
        });
    }
    #[test]
    fn report_reader_accepts_fragmented_bodies_and_rejects_envelopes_or_truncation(){
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use tokio::io::AsyncWriteExt;
            let(mut client,mut peer)=tokio::io::duplex(16);
            tokio::spawn(async move {
                // Two concatenated report bodies fragmented across arbitrary read boundaries.
                for part in [0xa2,1,0,3,9,0xa2,1,0,3,10].chunks(3) {
                    peer.write_all(part).await.unwrap();tokio::task::yield_now().await;
                }
                peer.shutdown().await.unwrap();
            });
            read_nan_stream(&mut client,Duration::from_secs(1)).await.unwrap();
            for data in [&[0xa1,1,46][..],&[0xa1,1][..],&[0xa1,1,0,0xa1][..],&[0xff][..]] {
                let(mut client,mut peer)=tokio::io::duplex(64);
                peer.write_all(data).await.unwrap();peer.shutdown().await.unwrap();
                assert!(read_nan_stream(&mut client,Duration::from_secs(1)).await.is_err());
            }
        });
    }
}
