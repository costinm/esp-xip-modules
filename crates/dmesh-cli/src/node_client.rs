//! Host client adapter over `quic-link` (`TokioAssocDriver`) and `quic-mux` (`TokioMux`).

use std::{
    future::Future,
    io::{Read, Write},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use quic_base::DEFAULT_HEADROOM;
use quic_link::{
    AssocConfig, ConnectionId, LinkId, LinkName, LinkProfile, PeerL2Address, TokioAssocDriver,
    TokioUdpLink, wildcard_bind,
};
use quic_mux::{MuxStream, TokioMux, TokioMuxConfig};

/// Maximum QUIC packet size accepted by `xip_runtime` (`XIP_QUIC_MAX_PACKET = 1448`).
const XIP_QUIC_LINK_MTU: usize = 1448;
/// Maximum inner `quic-mux` frame size leaving headroom for `ShortHeader` and piggybacked `ACK`s.
const XIP_QUIC_MUX_MTU: usize = 1380;

pub(crate) struct Response {
    pub bytes: Vec<u8>,
}

fn new_driver() -> TokioAssocDriver {
    TokioAssocDriver::new(AssocConfig::default(), 64)
}

async fn run_driver<T, F, Fut>(driver: TokioAssocDriver, operation: F) -> Result<T, String>
where
    F: FnOnce(TokioMux) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let (tx, rx, driver_task) = driver.spawn();
    let (mux, _acceptor) = TokioMux::spawn(true, TokioMuxConfig::default());
    mux.add_packet_link(tx, rx, XIP_QUIC_MUX_MTU, 10);
    let op_mux = mux.clone();
    let operation = async move {
        let result = operation(op_mux).await;
        drop(mux);
        result
    };
    tokio::select! {
        res = driver_task => match res {
            Ok(()) => Err("QUIC link driver stopped unexpectedly".to_owned()),
            Err(e) => Err(format!("QUIC link driver panicked: {e}")),
        },
        res = operation => res,
    }
}

fn attach_uart(
    driver: &mut TokioAssocDriver,
    id: LinkId,
    peer_cid: ConnectionId,
    path: &str,
    baud: Option<u32>,
) -> Result<(), String> {
    crate::xip_wire::XipPort::ensure_baud(path, baud)?;
    let mut port = uart_codec::host::UartPort::open(path, baud).map_err(|error| error.to_string())?;
    let _ = port.discard_input_blocking(Duration::ZERO);
    let mut bearer = port
        .into_tokio(LinkName::new("uart").unwrap())
        .map_err(|error| error.to_string())?;
    bearer.set_sideband_handler(|received| {
        for line in received.logs {
            eprintln!("dmesh_uart_console {line:?}");
        }
        for bytes in received.log_records {
            // Native NAN reports are a separate PPP channel, consumed by an
            // explicit watch. They are not malformed CBOR or stream diagnostics.
            if bytes.first() == Some(&(crate::xip_wire::PORT_NAN_REPORT as u8)) {
                continue;
            }
            eprintln!("{}", uart_codec::host::render_sideband_record(&bytes));
        }
        if received.pool_drops != 0 {
            eprintln!("dmesh_uart_log_drop pool={}", received.pool_drops);
        }
    });
    let mut profile =
        LinkProfile::udp_datagram(1, XIP_QUIC_LINK_MTU, peer_cid).with_encryption(false);
    profile.congestion_mode = quic_link::CongestionMode::FlowControlled {
        max_inflight_bytes: 6 * 1024,
        max_inflight_packets: 4,
    };
    profile.initial_rtt_us = 150_000;
    driver
        .add_link_with(
            id,
            PeerL2Address::new(1).unwrap(),
            profile,
            |pool, ingress| {
                bearer.attach(pool, DEFAULT_HEADROOM, ingress)?;
                Ok::<_, std::io::Error>(bearer)
            },
        )
        .map_err(|error| format!("attach UART link: {error}"))
}

fn attach_udp(
    driver: &mut TokioAssocDriver,
    id: LinkId,
    peer_cid: ConnectionId,
    bind: SocketAddr,
    peer: SocketAddr,
) -> Result<(), String> {
    // The fixed client port can be busy (a second CLI running): then any free port will do.
    let socket = match std::net::UdpSocket::bind(bind) {
        Ok(socket) => socket,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && bind.port() != 0 => {
            let mut any = bind;
            any.set_port(0);
            std::net::UdpSocket::bind(any).map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    let udp_link = TokioUdpLink::from_std(socket).map_err(|error| error.to_string())?;
    driver.add_udp_link(
        id,
        udp_link,
        peer,
        LinkProfile::udp_datagram(1, XIP_QUIC_LINK_MTU, peer_cid),
    );
    Ok(())
}

async fn exchange(mux: &TokioMux, payload: &[u8]) -> Result<Response, String> {
    let mut request = mux
        .open_bidi()
        .await
        .map_err(|error| format!("open request stream: {error}"))?;
    let bytes = quic_tools::stream_client::exchange(&mut request, &[payload])
        .await
        .map_err(|error| format!("request stream exchange: {error}"))?;
    Ok(Response { bytes })
}

pub(crate) async fn request_udp(
    bind: SocketAddr,
    peer: SocketAddr,
    payload: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let future = async {
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        attach_udp(&mut driver, LinkId(1), peer_cid, bind, peer)?;
        run_driver(driver, |mux| async move { exchange(&mux, payload).await }).await
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
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        attach_udp(&mut driver, LinkId(1), peer_cid, bind, peer)?;
        if let Some(path) = uart_path {
            attach_uart(&mut driver, LinkId(2), peer_cid, path, uart_baud)?;
        }
        run_driver(driver, |mux| async move { exchange(&mux, payload).await }).await
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("UDP stream request timed out after {timeout:?}"))?
}

#[cfg(test)]
pub(crate) async fn probe_udp(
    bind: SocketAddr,
    peer: SocketAddr,
    payload: &[u8],
    request: quic_tools::ProbeServiceRequest,
    timeout: Duration,
) -> Result<quic_tools::StreamProbeResult, String> {
    probe_udp_with_uart(bind, peer, None, None, payload, request, timeout).await
}

pub(crate) async fn probe_udp_with_uart(
    bind: SocketAddr,
    peer: SocketAddr,
    uart_path: Option<&str>,
    uart_baud: Option<u32>,
    payload: &[u8],
    request: quic_tools::ProbeServiceRequest,
    timeout: Duration,
) -> Result<quic_tools::StreamProbeResult, String> {
    let future = async {
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        attach_udp(&mut driver, LinkId(1), peer_cid, bind, peer)?;
        if let Some(path) = uart_path {
            attach_uart(&mut driver, LinkId(2), peer_cid, path, uart_baud)?;
        }
        run_driver(driver, |mux| async move {
            exchange_probe(&mux, payload, request).await
        })
        .await
    };
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("UDP QUIC probe timed out after {timeout:?}"))?
}

pub(crate) async fn probe_uart(
    path: &str,
    baud: Option<u32>,
    payload: &[u8],
    request: quic_tools::ProbeServiceRequest,
    timeout: Duration,
    linger: Duration,
) -> Result<quic_tools::StreamProbeResult, String> {
    let mut driver = new_driver();
    let peer_cid = ConnectionId::new(1).unwrap();
    attach_uart(&mut driver, LinkId(1), peer_cid, path, baud)?;
    run_driver(driver, |mux| async move {
        let attempt = exchange_probe(&mux, payload, request);
        monitor_uart_probe(attempt, timeout, linger).await
    })
    .await
}

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

async fn exchange_probe(
    mux: &TokioMux,
    payload: &[u8],
    request: quic_tools::ProbeServiceRequest,
) -> Result<quic_tools::StreamProbeResult, String> {
    quic_tools::stream_client::probe_mux(mux, payload, request)
        .await
        .map_err(|error| format!("probe stream exchange: {error}"))
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
    let mut driver = new_driver();
    let peer_cid = ConnectionId::new(1).unwrap();
    attach_uart(&mut driver, LinkId(1), peer_cid, path, baud)?;
    run_driver(driver, |mux| async move {
        loop {
            let request = payload()?;
            let response = match tokio::time::timeout(timeout, exchange(&mux, &request)).await {
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
            if let Some(interval) = repeat_interval {
                tokio::time::sleep(interval).await;
            } else {
                tokio::time::sleep(linger).await;
                return Ok(response);
            }
        }
    })
    .await
}

/// Upload on one native module stream, optionally through an explicit circuit.
pub(crate) async fn upload(
    target: &str,
    baud: Option<u32>,
    package: &[u8],
    via: Option<crate::via::Via>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let operation = async {
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        if let Some(via) = &via {
            via.attach(&mut driver, LinkId(1), peer_cid, baud)?;
        } else if target.starts_with("udp://") {
            let peer = crate::client::parse_udp_peer(target)?;
            attach_udp(&mut driver, LinkId(1), peer_cid, wildcard_bind(peer, 0), peer)?;
        } else {
            attach_uart(&mut driver, LinkId(1), peer_cid, target, baud)?;
        }
        run_driver(driver, |mux| async move {
            let stream = mux
                .open_bidi()
                .await
                .map_err(|e| format!("upload stream: {e}"))?;
            let (mut rx, mut tx) = tokio::io::split(stream);
            // One self-delimiting envelope followed by the unchanged WLU1 body.
            let mut e = minicbor::Encoder::new(Vec::new());
            e.map(4)
                .unwrap()
                .u8(1)
                .unwrap()
                .u8(44)
                .unwrap()
                .u8(2)
                .unwrap()
                .u8(1)
                .unwrap()
                .u8(3)
                .unwrap()
                .u64(crate::client::fresh_request_id() & i64::MAX as u64)
                .unwrap()
                .u8(5)
                .unwrap()
                .map(0)
                .unwrap();
            let sending = async {
                tx.write_all(e.writer()).await?;
                tx.write_all(package).await?;
                tx.shutdown().await
            };
            let receiving = async {
                let mut bytes = Vec::new();
                (&mut rx)
                    .take(513)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                if bytes.len() > 512 {
                    return Err("upload response exceeds 512 bytes".to_string());
                }
                Ok(bytes)
            };
            tokio::pin!(sending, receiving);
            let response = tokio::select! {
                reply = &mut receiving => reply?,
                sent = &mut sending => {
                    let reply = receiving.await?;
                    if reply.is_empty() {
                        sent.map_err(|e| e.to_string())?;
                    }
                    reply
                }
            };
            Ok(response)
        })
        .await
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| format!("QUIC upload timed out after {timeout:?}"))?
}

pub(crate) async fn native_probe(
    target: &str,
    baud: Option<u32>,
    via: Option<crate::via::Via>,
    request: quic_tools::ProbeServiceRequest,
    timeout: Duration,
    _stats: bool,
) -> Result<quic_tools::StreamProbeResult, String> {
    let mut header = [0; quic_tools::PROBE_REQUEST_MAX];
    let id = crate::client::fresh_request_id() & i64::MAX as u64;
    let used = quic_tools::encode_probe_handler_request(request, id, &mut header)
        .ok_or("probe header too large")?;
    let operation = async {
        if target.starts_with("tcp://") && via.is_none() {
            let peer = crate::client::parse_udp_peer(target.trim_start_matches("tcp://"))?;
            let mut socket = tokio::net::TcpStream::connect(peer)
                .await
                .map_err(|e| e.to_string())?;
            socket.set_nodelay(true).map_err(|e| e.to_string())?;
            return if request.direction == quic_tools::ProbeDirection::Both {
                let mut down = tokio::net::TcpStream::connect(peer)
                    .await
                    .map_err(|e| e.to_string())?;
                quic_tools::stream_client::probe_duplex(&mut socket, &mut down, request, id).await
            } else {
                quic_tools::stream_client::probe(&mut socket, &header[..used], request).await
            }
            .map_err(|e| e.to_string());
        }
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        if let Some(via) = &via {
            via.attach(&mut driver, LinkId(1), peer_cid, baud)?;
        } else if target.starts_with("udp://") {
            let peer = crate::client::parse_udp_peer(target)?;
            attach_udp(&mut driver, LinkId(1), peer_cid, wildcard_bind(peer, 0), peer)?;
        } else {
            attach_uart(&mut driver, LinkId(1), peer_cid, target, baud)?;
        }
        run_driver(driver, |mux| async move {
            exchange_probe(&mux, &header[..used], request).await
        })
        .await
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| format!("probe timed out after {timeout:?}"))?
}

/// Read report bodies over the same module stream interface used by flash/probes.
pub(crate) async fn nan_stream(
    target: &str,
    baud: Option<u32>,
    via: Option<crate::via::Via>,
    request: &[u8],
    duration: Duration,
) -> Result<(), String> {
    if target.starts_with("tcp://") && via.is_none() {
        return Err("NAN has no direct TCP listener; use a QUIC bearer".into());
    }
    let mut driver = new_driver();
    let peer_cid = ConnectionId::new(1).unwrap();
    if let Some(via) = &via {
        via.attach(&mut driver, LinkId(1), peer_cid, baud)?;
    } else if target.starts_with("udp://") {
        let peer = crate::client::parse_udp_peer(target)?;
        attach_udp(&mut driver, LinkId(1), peer_cid, wildcard_bind(peer, 0), peer)?;
    } else {
        attach_uart(&mut driver, LinkId(1), peer_cid, target, baud)?;
    }
    run_driver(driver, |mux| async move {
        let mut stream = mux
            .open_bidi()
            .await
            .map_err(|e| format!("open NAN stream: {e}"))?;
        write_nan_watch(&mut stream, request).await?;
        read_nan_stream(&mut stream, duration).await
    })
    .await
}

// The watch has no body: flush the opening request and half-close TX, retaining RX.
async fn write_nan_watch(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    request: &[u8],
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(request).await.map_err(|e| e.to_string())?;
    stream.shutdown().await.map_err(|e| e.to_string())
}

async fn read_nan_stream(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    duration: Duration,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let deadline = tokio::time::Instant::now() + duration;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 1500];
    let mut records = 0;
    loop {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Err(_) => break,
            Ok(Err(e)) => return Err(e.to_string()),
            Ok(Ok(n)) => n,
        };
        if n == 0 {
            if !pending.is_empty() {
                return Err("NAN stream ended in a partial CBOR record".into());
            }
            break;
        }
        pending.extend_from_slice(&chunk[..n]);
        loop {
            let mut decoder = minicbor::Decoder::new(&pending);
            match decoder.skip() {
                Ok(()) => {}
                Err(e) if e.is_end_of_input() => break,
                Err(e) => return Err(format!("invalid NAN report CBOR: {e}")),
            }
            let n = decoder.position();
            if n == 0 {
                return Err("empty NAN record".into());
            }
            let record = crate::console::report(&pending[..n])?;
            println!("{record}");
            records += 1;
            pending.drain(..n);
        }
        if pending.len() > 1500 {
            return Err("NAN stream record exceeds one packet".into());
        }
    }
    if records == 0 {
        Err("NAN stream returned no records".into())
    } else if !pending.is_empty() {
        Err("NAN stream ended in a partial CBOR record".into())
    } else {
        Ok(())
    }
}

struct CachedQuicSession {
    endpoint: String,
    rt: Arc<tokio::runtime::Runtime>,
    mux: TokioMux,
    driver_task: tokio::task::JoinHandle<()>,
}

static CACHED_QUIC_SESSION: Mutex<Option<CachedQuicSession>> = Mutex::new(None);

pub(crate) fn clear_cached_flash_session(endpoint: &str) {
    if let Ok(mut guard) = CACHED_QUIC_SESSION.lock() {
        if guard.as_ref().is_some_and(|s| s.endpoint == endpoint) {
            if let Some(session) = guard.take() {
                session.mux.close();
                session.driver_task.abort();
                let _ = session.rt.block_on(session.driver_task);
            }
        }
    }
}

fn get_flash_session(endpoint: &str) -> Result<(Arc<tokio::runtime::Runtime>, TokioMux), String> {
    let mut guard = CACHED_QUIC_SESSION
        .lock()
        .map_err(|_| "QUIC session mutex poisoned".to_owned())?;
    if let Some(session) = guard.as_ref() {
        if session.endpoint == endpoint && !session.driver_task.is_finished() {
            return Ok((session.rt.clone(), session.mux.clone()));
        }
    }
    *guard = None;

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?,
    );
    let target = endpoint.to_owned();
    let (mux, driver_task) = rt.block_on(async move {
        let mut driver = new_driver();
        let peer_cid = ConnectionId::new(1).unwrap();
        if target.starts_with("udp://") {
            let peer = crate::client::parse_udp_peer(&target)?;
            attach_udp(&mut driver, LinkId(1), peer_cid, wildcard_bind(peer, 0), peer)?;
        } else {
            attach_uart(&mut driver, LinkId(1), peer_cid, &target, None)?;
        }
        let (tx, rx, driver_task) = driver.spawn();
        let (mux, _acceptor) = TokioMux::spawn(true, TokioMuxConfig::default());
        mux.add_packet_link(tx, rx, XIP_QUIC_MUX_MTU, 10);
        Ok::<_, String>((mux, driver_task))
    })?;
    *guard = Some(CachedQuicSession {
        endpoint: endpoint.to_owned(),
        rt: rt.clone(),
        mux: mux.clone(),
        driver_task,
    });
    Ok((rt, mux))
}

pub(crate) struct QuicFlashStream {
    endpoint: String,
    rt: Arc<tokio::runtime::Runtime>,
    stream: Option<MuxStream>,
    write_shutdown: bool,
    errored: bool,
}

impl QuicFlashStream {
    pub(crate) fn shutdown_write(&mut self) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        if self.write_shutdown {
            return Ok(());
        }
        let Some(stream) = self.stream.as_mut() else {
            return Err("QUIC flash stream is closed".to_owned());
        };
        let res = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(60), stream.shutdown()).await });
        match res {
            Ok(Ok(())) => {
                self.write_shutdown = true;
                Ok(())
            }
            Ok(Err(e)) => {
                self.errored = true;
                Err(format!("QUIC stream shutdown: {e}"))
            }
            Err(_) => {
                self.errored = true;
                Err("QUIC stream shutdown timed out".to_owned())
            }
        }
    }
}

impl Read for QuicFlashStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncReadExt;
        if let Err(err) = self.shutdown_write() {
            return Err(std::io::Error::other(err));
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(0);
        };
        let res = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(60), stream.read(buf)).await });
        match res {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => {
                self.errored = true;
                Err(e)
            }
            Err(_) => {
                self.errored = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "QUIC stream read timed out",
                ))
            }
        }
    }
}

impl Write for QuicFlashStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncWriteExt;
        let Some(stream) = self.stream.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "QUIC flash stream is closed",
            ));
        };
        let res = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(60), stream.write(buf)).await });
        match res {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => {
                self.errored = true;
                Err(e)
            }
            Err(_) => {
                self.errored = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "QUIC stream write timed out",
                ))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let res = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(60), stream.flush()).await });
        match res {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.errored = true;
                Err(e)
            }
            Err(_) => {
                self.errored = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "QUIC stream flush timed out",
                ))
            }
        }
    }
}

impl Drop for QuicFlashStream {
    fn drop(&mut self) {
        drop(self.stream.take());
        self.rt
            .block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        if self.errored {
            clear_cached_flash_session(&self.endpoint);
        }
    }
}

pub(crate) fn open_flash_stream(endpoint: &str) -> Result<QuicFlashStream, String> {
    let (rt, mux) = get_flash_session(endpoint)?;
    let stream = match rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), mux.open_bidi()).await
    }) {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            clear_cached_flash_session(endpoint);
            return Err(format!("open QUIC flash stream: {e}"));
        }
        Err(_) => {
            clear_cached_flash_session(endpoint);
            return Err("open QUIC flash stream timed out".to_owned());
        }
    };
    Ok(QuicFlashStream {
        endpoint: endpoint.to_owned(),
        rt,
        stream: Some(stream),
        write_shutdown: false,
        errored: false,
    })
}

#[cfg(test)]
mod tests {
    use super::request_udp;
    use std::{
        net::{SocketAddr, UdpSocket},
        thread,
        time::Duration,
    };

    use quic_base::DEFAULT_MAX_PAYLOAD;
    use quic_link::{AssocConfig, LinkProfile, TokioUdpLink, spawn_udp_assoc_packet_link};
    use quic_mux::{TokioMux, TokioMuxConfig};

    async fn accept_server_mux(server_socket: UdpSocket) -> (TokioMux, quic_mux::TokioMuxAcceptor) {
        server_socket.set_nonblocking(true).unwrap();
        let tokio_sock = tokio::net::UdpSocket::from_std(server_socket).unwrap();
        let mut buf = [0u8; 1];
        let client_addr = loop {
            match tokio_sock.peek_from(&mut buf).await {
                Ok((_, addr)) => break addr,
                Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        };
        let udp_link = TokioUdpLink::from_std(tokio_sock.into_std().unwrap()).unwrap();
        let cfg = AssocConfig::default();
        let profile = LinkProfile::udp_datagram(1, DEFAULT_MAX_PAYLOAD, cfg.default_peer_cid);
        let (tx, rx, _task) = spawn_udp_assoc_packet_link(udp_link, client_addr, cfg, profile, 64);
        let (mux, acceptor) = TokioMux::spawn(false, TokioMuxConfig::default());
        mux.add_packet_link(tx, rx, DEFAULT_MAX_PAYLOAD, 10);
        (mux, acceptor)
    }

    #[test]
    fn udp_quic_probe_request_returns_error_for_unsupported_stream() {
        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server_socket.local_addr().unwrap();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (_mux, mut acceptor) = accept_server_mux(server_socket).await;
                let mut stream =
                    tokio::time::timeout(Duration::from_secs(3), acceptor.accept())
                        .await
                        .expect("server did not accept the UDP QUIC probe")
                        .expect("server stream acceptance failed");
                crate::mux_cli::default_mux_server_handler()
                    .serve(&mut stream)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
        });

        let mut request_wire = [0u8; quic_tools::PROBE_REQUEST_MAX];
        let request_len = quic_tools::encode_probe_start_request(
            quic_tools::ProbeServiceRequest::new(4096, 512),
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

        let record = mesh::mesh_api::envelope::decode(&response.bytes).unwrap();
        assert_eq!(record.component, Some(mesh::mesh_api::envelope::Name::Tag(8)));
        assert_eq!(record.method, Some(mesh::mesh_api::envelope::Name::Tag(1)));
        assert_eq!(record.id, Some(0x1234));
        assert!(record.error.is_some());
    }

    #[test]
    fn udp_probe_stream_validates_the_peer_payload_and_completion() {
        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server_socket.local_addr().unwrap();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let service = crate::mux_cli::default_mux_server_handler();
                for index in 0..3 {
                    let s = server_socket.try_clone().unwrap();
                    let (_mux, mut acceptor) = accept_server_mux(s).await;
                    let mut stream =
                        tokio::time::timeout(Duration::from_secs(3), acceptor.accept())
                            .await
                            .expect("server did not accept the UDP probe")
                            .expect("server stream acceptance failed");
                    if index == 2 {
                        let mut second = acceptor.accept().await.unwrap();
                        tokio::try_join!(
                            service.serve(&mut stream),
                            service.serve(&mut second)
                        )
                        .unwrap();
                    } else {
                        service.serve(&mut stream).await.unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        });

        for direction in [
            quic_tools::ProbeDirection::Download,
            quic_tools::ProbeDirection::Upload,
            quic_tools::ProbeDirection::Both,
        ] {
            let mut probe_request = quic_tools::ProbeServiceRequest::new(256 * 1024, 512);
            probe_request.direction = direction;
            let mut request_wire = [0u8; quic_tools::PROBE_REQUEST_MAX];
            let request_len = quic_tools::encode_probe_handler_request(
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

#[cfg(test)]
mod nan_stream_tests {
    use super::*;
    #[test]
    fn watch_half_closes_only_the_sending_direction() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (client, mut peer) = tokio::io::duplex(64);
            let mut client = tokio::io::BufWriter::new(client);
            write_nan_watch(&mut client, b"open").await.unwrap();
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"open");
            peer.write_all(b"report").await.unwrap();
            let mut received = [0; 6];
            client.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"report");
        });
    }
    #[test]
    fn report_reader_accepts_fragmented_bodies_and_rejects_envelopes_or_truncation() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use tokio::io::AsyncWriteExt;
            let (mut client, mut peer) = tokio::io::duplex(16);
            tokio::spawn(async move {
                // Two concatenated report bodies fragmented across arbitrary read boundaries.
                for part in [0xa2, 1, 0, 3, 9, 0xa2, 1, 0, 3, 10].chunks(3) {
                    peer.write_all(part).await.unwrap();
                    tokio::task::yield_now().await;
                }
                peer.shutdown().await.unwrap();
            });
            read_nan_stream(&mut client, Duration::from_secs(1))
                .await
                .unwrap();
            for data in [&[0xa1, 1, 46][..], &[0xa1, 1][..], &[0xa1, 1, 0, 0xa1][..], &[0xff][..]] {
                let (mut client, mut peer) = tokio::io::duplex(64);
                peer.write_all(data).await.unwrap();
                peer.shutdown().await.unwrap();
                assert!(read_nan_stream(&mut client, Duration::from_secs(1)).await.is_err());
            }
        });
    }
}
