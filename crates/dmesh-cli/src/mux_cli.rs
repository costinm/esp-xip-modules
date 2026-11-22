//! Standalone `quic-mux` client and server support across TCP (`mux://HOST:PORT`),
//! Unix Domain Sockets (`mux+unix:///path`), and PPP serial (`PORT_MUX = 245`),
//! including multipath sessions combining TCP/UDS with PPP serial.

use crate::{
    client::{fresh_request_id, is_fatal_diagnostic, parse_udp_peer, render_stream_response},
    device::load_device,
    schema::{encode_stream_argv_with_id, is_stream_command_name},
    xip_wire::{Event, PORT_MUX, Reader, XipPort, encode_record},
};
pub use quic_mux::{AssociationId, TokioMux, TokioMuxAcceptor, TokioMuxConfig};
pub use quic_tools::{ProbeDirection, ProbeServiceRequest};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    sync::mpsc,
};

/// Default TCP port for `mod_mux` (`--stream 4` => `5225 + 4 = 5229`).
pub const DEFAULT_MUX_TCP_PORT: u16 = 5229;

/// Maximum `quic-mux` packet payload on PPP serial port `245` (`1500` record).
pub const PPP_MUX_MTU: usize = 1500;

/// Endpoint selection for a `quic-mux` client session.
#[derive(Clone, Debug, Default)]
pub struct MuxClientTarget {
    pub tcp: Option<SocketAddr>,
    pub uds: Option<PathBuf>,
    pub serial: Option<String>,
    pub baud: Option<u32>,
}

impl MuxClientTarget {
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(tcp) = self.tcp {
            parts.push(format!("mux://{tcp}"));
        }
        if let Some(uds) = &self.uds {
            parts.push(format!("mux+unix://{}", uds.display()));
        }
        if let Some(serial) = &self.serial {
            parts.push(format!("mux+serial://{serial}"));
        }
        parts.join("+")
    }

    pub fn bearer_name(&self) -> &'static str {
        match (self.tcp.is_some(), self.uds.is_some(), self.serial.is_some()) {
            (true, false, true) => "mux-tcp+uart",
            (false, true, true) => "mux-uds+uart",
            (true, true, _) => "mux-tcp+uds",
            (true, false, false) => "mux-tcp",
            (false, true, false) => "mux-uds",
            (false, false, true) => "mux-uart",
            (false, false, false) => "mux",
        }
    }
}

/// Parse a `mux://HOST[:PORT]`, `mux+unix:///PATH`, or `mux+serial:///PATH` target string.
pub fn parse_mux_target_uri(target: &str) -> Result<Option<MuxClientTarget>, String> {
    if let Some(rest) = target.strip_prefix("mux://") {
        let addr = quic_link::parse_udp_peer(rest, DEFAULT_MUX_TCP_PORT)
            .map_err(|e| e.to_string())?;
        return Ok(Some(MuxClientTarget {
            tcp: Some(addr),
            ..Default::default()
        }));
    }
    if let Some(rest) = target.strip_prefix("mux+unix://") {
        if rest.is_empty() {
            return Err("mux+unix:// requires a socket path".into());
        }
        return Ok(Some(MuxClientTarget {
            uds: Some(PathBuf::from(rest)),
            ..Default::default()
        }));
    }
    if let Some(rest) = target.strip_prefix("mux+serial://") {
        if rest.is_empty() {
            return Err("mux+serial:// requires a serial path or device name".into());
        }
        let (serial, baud) = resolve_serial_selector(rest, None)?;
        return Ok(Some(MuxClientTarget {
            serial: Some(serial),
            baud,
            ..Default::default()
        }));
    }
    Ok(None)
}

fn resolve_serial_selector(
    selector: &str,
    explicit_baud: Option<u32>,
) -> Result<(String, Option<u32>), String> {
    if selector.starts_with('/') {
        return Ok((selector.to_owned(), explicit_baud));
    }
    let profile = load_device(selector)?;
    let serial = profile
        .serial_path()?
        .ok_or_else(|| format!("device {selector:?} has no serial_id"))?;
    Ok((
        serial.display().to_string(),
        explicit_baud.or(profile.uart_baud),
    ))
}

/// Exclusive PPP serial pump on `PORT_MUX` (`245`).
///
/// Preserves the non-resetting `XipPort` open discipline and forwards device log text lines
/// while pumping `quic-mux` packet frames between `TokioMux` and the serial wire.
pub struct SerialMuxGuard {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl SerialMuxGuard {
    pub fn spawn(
        mut port: XipPort,
        inbound_tx: mpsc::Sender<Vec<u8>>,
        mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut reader = Reader::default();
            let mut fatal: Option<String> = None;
            while !stop_bg.load(Ordering::Acquire) {
                let mut did_work = false;
                while let Ok(frame) = outbound_rx.try_recv() {
                    let wire = encode_record(PORT_MUX, &frame);
                    port.write_all(&wire).map_err(|e| e.to_string())?;
                    did_work = true;
                }
                let bytes = port.read().map_err(|e| e.to_string())?;
                if !bytes.is_empty() {
                    did_work = true;
                    for ev in reader.feed(&bytes) {
                        if let Event::Record(p, payload) = ev {
                            if p == PORT_MUX && inbound_tx.blocking_send(payload).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    let text = reader.take_text();
                    if !text.is_empty() {
                        let s = String::from_utf8_lossy(&text);
                        if is_fatal_diagnostic(&s) {
                            fatal = Some(format!("device reported fatal diagnostic: {}", s.trim()));
                            break;
                        }
                    }
                }
                if !did_work {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            if let Some(err) = fatal {
                return Err(err);
            }
            Ok(())
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            return h.join().map_err(|_| "serial mux worker panicked".to_owned())?;
        }
        Ok(())
    }
}

impl Drop for SerialMuxGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Connect a `TokioMux` client session across all bearers configured in `target`.
pub async fn connect_mux_client(
    target: &MuxClientTarget,
    config: TokioMuxConfig,
) -> Result<(TokioMux, TokioMuxAcceptor, Option<SerialMuxGuard>), String> {
    if target.tcp.is_none() && target.uds.is_none() && target.serial.is_none() {
        return Err("mux client requires at least one of TCP, UDS, or serial bearer".into());
    }
    let (mux, acceptor) = TokioMux::spawn(true, config);
    if let Some(addr) = target.tcp {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect mux TCP {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        mux.add_tcp_link(stream, 10);
    }
    if let Some(path) = &target.uds {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("connect mux UDS {}: {e}", path.display()))?;
        mux.add_uds_stream_link(stream, 10);
    }
    let mut serial_guard = None;
    if let Some(serial_path) = &target.serial {
        let port = XipPort::open_ensuring_baud(serial_path.clone(), target.baud)
            .map_err(|e| format!("open serial {serial_path}: {e}"))?;
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        mux.add_packet_link(out_tx, in_rx, PPP_MUX_MTU, 20);
        serial_guard = Some(SerialMuxGuard::spawn(port, in_tx, out_rx));
    }
    Ok((mux, acceptor, serial_guard))
}

/// Execute a tagged CBOR stream exchange over an open `TokioMux` client session.
pub async fn mux_exchange(mux: &TokioMux, request: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = mux.open_bidi().await.map_err(|e| e.to_string())?;
    quic_tools::exchange(&mut stream, &[request])
        .await
        .map_err(|e| e.to_string())
}

/// Execute a `mesh::wire` self-delimiting CBOR record + optional raw body exchange
/// over an open `TokioMux` client session (`mesh-cli` wire compatible).
pub async fn mux_exchange_cbor_stream(
    mux: &TokioMux,
    request: &mesh::tagged::TaggedRecord,
    body: &[u8],
) -> Result<mesh::wire::CborStreamFrame, String> {
    let mut stream = mux.open_bidi().await.map_err(|e| e.to_string())?;
    mesh::wire::exchange_cbor_stream(&mut stream, request, body)
        .await
        .map_err(|e| e.to_string())
}

/// Execute a validated `probe` (`upload`, `download`, or `both`) over an open `TokioMux` client session.
pub async fn mux_probe(
    mux: &TokioMux,
    request: quic_tools::ProbeServiceRequest,
    id: u64,
) -> Result<quic_tools::StreamProbeResult, String> {
    let mut header = [0u8; quic_tools::PROBE_REQUEST_MAX];
    let used = quic_tools::encode_probe_handler_request(request, id, &mut header)
        .ok_or("probe header does not fit")?;
    quic_tools::probe_mux(mux, &header[..used], request)
        .await
        .map_err(|e| e.to_string())
}

/// Stream responder for `dmesh-cli mux-listen` (`probe.handler` and `status`).
pub struct MuxServerHandler;

impl MuxServerHandler {
    pub async fn serve<S>(&self, stream: &mut S) -> std::io::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use mesh::mesh_api::envelope::{Name, encode_numeric_error, encode_numeric_response};
        use tokio::io::AsyncWriteExt;

        async fn finish_unary<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
            stream: &mut S,
        ) -> std::io::Result<()> {
            stream.shutdown().await?;
            // Wait briefly for the peer's FIN so dropping `MuxStream` does not
            // emit an unnecessary `STOP_SENDING(0)` while `writer.shutdown()` is in flight.
            let _ = tokio::time::timeout(
                Duration::from_millis(50),
                tokio::io::copy(stream, &mut tokio::io::sink()),
            )
            .await;
            Ok(())
        }

        let (envelope, mut framed) = mesh::read_envelope_prefix(stream)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let Some(record) = envelope.record() else {
            return finish_unary(&mut framed).await;
        };
        let id = record.id.unwrap_or(0);
        let (comp, meth, fields, valid_stream) = (
            record.component,
            record.method,
            record.fields.map(|f| f.to_vec()),
            record.params.is_none() && record.to.is_none(),
        );
        match (comp, meth) {
            (Some(Name::Tag(8)) | Some(Name::Text(b"probe")), Some(Name::Tag(2)) | Some(Name::Text(b"handler")))
                if valid_stream =>
            {
                quic_tools::serve_framed_probe_stream(id, fields.as_deref(), &mut framed).await
            }
            (
                Some(Name::Tag(9)) | Some(Name::Text(b"diagnostic")),
                Some(Name::Tag(1)) | Some(Name::Text(b"status")),
            ) => {
                let mut out = [0u8; 64];
                if let Some(used) = encode_numeric_response(
                    9,
                    1,
                    id,
                    &[0xa1, 1, 0x69, b'd', b'm', b'e', b's', b'h', b'-', b'c', b'l', b'i'],
                    &mut out,
                ) {
                    framed.write_all(&out[..used]).await?;
                }
                finish_unary(&mut framed).await
            }
            _ => {
                let comp_id = match comp {
                    Some(Name::Tag(c)) => c,
                    _ => 0,
                };
                let meth_id = match meth {
                    Some(Name::Tag(m)) => m,
                    _ => 0,
                };
                let mut out = [0u8; 96];
                if let Some(used) =
                    encode_numeric_error(comp_id, meth_id, id, b"unsupported method", &mut out)
                {
                    framed.write_all(&out[..used]).await?;
                }
                finish_unary(&mut framed).await
            }
        }
    }
}

/// Build the default `MuxServerHandler` used by `dmesh-cli mux-listen` server mode.
pub fn default_mux_server_handler() -> Arc<MuxServerHandler> {
    Arc::new(MuxServerHandler)
}

/// Serve all accepted `MuxStream`s from `acceptor` using `handler` until the session closes.
pub async fn serve_mux_acceptor(
    _mux_guard: TokioMux,
    mut acceptor: TokioMuxAcceptor,
    handler: Arc<MuxServerHandler>,
) {
    while let Some(mut stream) = acceptor.accept().await {
        let h = handler.clone();
        tokio::spawn(async move {
            let _ = h.serve(&mut stream).await;
        });
    }
}

/// Configuration for `dmesh-cli mux-listen` server mode.
#[derive(Default)]
pub struct MuxServerConfig {
    pub tcp: Option<SocketAddr>,
    pub uds: Option<PathBuf>,
    pub serial: Option<String>,
    pub baud: Option<u32>,
    pub mux_config: TokioMuxConfig,
}

/// Running `quic-mux` server handle with bound local addresses and clean shutdown.
pub struct RunningMuxServer {
    pub tcp_addr: Option<SocketAddr>,
    pub uds_path: Option<PathBuf>,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    join: tokio::task::JoinHandle<Result<(), String>>,
}

impl RunningMuxServer {
    pub async fn stop(mut self) -> Result<(), String> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        let res = self.join.await.map_err(|e| e.to_string())?;
        if let Some(path) = &self.uds_path {
            let _ = std::fs::remove_file(path);
        }
        res
    }
}

/// Start a `quic-mux` server on the configured TCP, UDS, and/or PPP serial endpoints.
pub async fn start_mux_server(
    config: MuxServerConfig,
    handler: Arc<MuxServerHandler>,
) -> Result<RunningMuxServer, String> {
    if config.tcp.is_none() && config.uds.is_none() && config.serial.is_none() {
        return Err("mux-listen requires at least one of --tcp, --uds, or --serial".into());
    }

    let tcp_listener = match config.tcp {
        Some(addr) => Some(
            TcpListener::bind(addr)
                .await
                .map_err(|e| format!("bind mux TCP {addr}: {e}"))?,
        ),
        None => None,
    };
    let tcp_addr = match &tcp_listener {
        Some(l) => Some(l.local_addr().map_err(|e| e.to_string())?),
        None => None,
    };

    let uds_listener = match &config.uds {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
            }
            let _ = std::fs::remove_file(path);
            Some(
                UnixListener::bind(path)
                    .map_err(|e| format!("bind mux UDS {}: {e}", path.display()))?,
            )
        }
        None => None,
    };
    let uds_path = config.uds.clone();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let mux_cfg = config.mux_config;
    let serial_cfg = config.serial;
    let serial_baud = config.baud;

    // When a serial bearer is attached alongside TCP/UDS on the server, attach incoming TCP/UDS
    // links to the same shared server `TokioMux` so a multipath client can stripe/failover across
    // both bearers into one server session! Otherwise each independent TCP/UDS connection gets its
    // own isolated `TokioMux::server` session.
    let join = tokio::spawn(async move {
        let mut serial_guard = None;
        let mut shared_server_mux: Option<TokioMux> = None;

        if let Some(serial_path) = serial_cfg {
            let port = XipPort::open_ensuring_baud(serial_path.clone(), serial_baud)
                .map_err(|e| format!("open serial {serial_path}: {e}"))?;
            let (mux, acceptor) = TokioMux::spawn(false, mux_cfg.clone());
            let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(64);
            let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
            mux.add_packet_link(out_tx, in_rx, PPP_MUX_MTU, 20);
            serial_guard = Some(SerialMuxGuard::spawn(port, in_tx, out_rx));
            shared_server_mux = Some(mux.clone());
            tokio::spawn(serve_mux_acceptor(mux, acceptor, handler.clone()));
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                res = async {
                    match &tcp_listener {
                        Some(l) => l.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (stream, _) = res.map_err(|e| e.to_string())?;
                    let _ = stream.set_nodelay(true);
                    if let Some(shared) = &shared_server_mux {
                        shared.add_tcp_link(stream, 10);
                    } else {
                        let (mux, acceptor) = TokioMux::server(stream, mux_cfg.clone());
                        tokio::spawn(serve_mux_acceptor(mux, acceptor, handler.clone()));
                    }
                }
                res = async {
                    match &uds_listener {
                        Some(l) => l.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (stream, _) = res.map_err(|e| e.to_string())?;
                    if let Some(shared) = &shared_server_mux {
                        shared.add_uds_stream_link(stream, 10);
                    } else {
                        let (mux, acceptor) = TokioMux::server(stream, mux_cfg.clone());
                        tokio::spawn(serve_mux_acceptor(mux, acceptor, handler.clone()));
                    }
                }
            }
        }

        if let Some(shared) = shared_server_mux {
            shared.close();
        }
        if let Some(mut guard) = serial_guard {
            guard.shutdown()?;
        }
        Ok(())
    });

    Ok(RunningMuxServer {
        tcp_addr,
        uds_path,
        shutdown_tx: Some(shutdown_tx),
        join,
    })
}

/// CLI entry point for `dmesh-cli mux-listen [--tcp ADDR] [--uds PATH] [--serial PATH] [--baud N] [--timeout-secs N]`.
pub fn run_server_cli(arguments: &[String]) -> Result<(), String> {
    let mut tcp = None;
    let mut uds = None;
    let mut serial = None;
    let mut baud = None;
    let mut timeout: Option<Duration> = None;

    let mut i = 1;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "--help" | "-h" => {
                println!(
                    "usage: dmesh-cli mux-listen [--tcp HOST:PORT] [--uds PATH] [--serial PATH] [--baud N] [--timeout-secs N]"
                );
                return Ok(());
            }
            "--tcp" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --tcp value")?;
                tcp = Some(
                    quic_link::parse_udp_peer(v, DEFAULT_MUX_TCP_PORT)
                        .map_err(|e| e.to_string())?,
                );
            }
            "--uds" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --uds value")?;
                uds = Some(PathBuf::from(v));
            }
            "--serial" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --serial value")?;
                serial = Some(v.clone());
            }
            "--baud" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --baud value")?;
                baud = Some(v.parse::<u32>().map_err(|e| e.to_string())?);
            }
            "--timeout-secs" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --timeout-secs value")?;
                let secs = v.parse::<u64>().map_err(|e| e.to_string())?;
                if secs == 0 {
                    return Err("--timeout-secs must be positive".into());
                }
                timeout = Some(Duration::from_secs(secs));
            }
            other => return Err(format!("unknown mux-listen argument {other:?}")),
        }
        i += 1;
    }

    if let Some(sel) = serial.take() {
        let (resolved, resolved_baud) = resolve_serial_selector(&sel, baud)?;
        serial = Some(resolved);
        baud = resolved_baud;
    }

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let server = start_mux_server(
            MuxServerConfig {
                tcp,
                uds,
                serial,
                baud,
                mux_config: TokioMuxConfig::default(),
            },
            default_mux_server_handler(),
        )
        .await?;
        println!(
            "dmesh_cli_mux_listen tcp={:?} uds={:?}",
            server.tcp_addr,
            server.uds_path.as_ref().map(|p| p.display().to_string())
        );
        match timeout {
            Some(dur) => tokio::time::sleep(dur).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        server.stop().await
    })
}

/// CLI entry point for `dmesh-cli mux://... | mux+unix://... | mux+serial://... METHOD [fields...] [--serial PATH] [--tcp HOST:PORT] [--uds PATH] [--baud N] [--timeout-secs N]`.
pub fn run_client_cli(arguments: &[String]) -> Result<(), String> {
    let first = arguments.first().ok_or("missing mux target")?;
    let mut target = parse_mux_target_uri(first)?
        .ok_or_else(|| format!("unsupported mux target {first:?}"))?;

    let mut service_args = Vec::new();
    let mut timeout_override: Option<Duration> = None;
    let mut extra_serial: Option<String> = None;
    let mut explicit_baud: Option<u32> = None;

    let mut i = 1;
    while i < arguments.len() {
        if let Some(v) = arguments[i].strip_prefix("--timeout-secs=") {
            let secs = v.parse::<u64>().map_err(|e| e.to_string())?;
            if secs == 0 {
                return Err("--timeout-secs must be positive".into());
            }
            timeout_override = Some(Duration::from_secs(secs));
            i += 1;
            continue;
        }
        match arguments[i].as_str() {
            "--timeout-secs" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --timeout-secs value")?;
                let secs = v.parse::<u64>().map_err(|e| e.to_string())?;
                if secs == 0 {
                    return Err("--timeout-secs must be positive".into());
                }
                timeout_override = Some(Duration::from_secs(secs));
            }
            "--serial" | "--uart-bearer" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --serial path")?;
                extra_serial = Some(v.clone());
            }
            "--tcp" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --tcp endpoint")?;
                target.tcp = Some(
                    parse_udp_peer(v).or_else(|_| {
                        quic_link::parse_udp_peer(v, DEFAULT_MUX_TCP_PORT)
                            .map_err(|e| e.to_string())
                    })?,
                );
            }
            "--uds" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --uds path")?;
                target.uds = Some(PathBuf::from(v));
            }
            "--baud" | "--uart-baud" => {
                i += 1;
                let v = arguments.get(i).ok_or("missing --baud value")?;
                explicit_baud = Some(v.parse::<u32>().map_err(|e| e.to_string())?);
            }
            other => service_args.push(other.to_owned()),
        }
        i += 1;
    }

    if let Some(sel) = extra_serial {
        let (resolved, resolved_baud) = resolve_serial_selector(&sel, explicit_baud)?;
        target.serial = Some(resolved);
        target.baud = resolved_baud;
    } else if explicit_baud.is_some() {
        target.baud = explicit_baud;
    }

    let method_name = service_args
        .first()
        .filter(|s| is_stream_command_name(s))
        .ok_or("missing schema service for mux target")?
        .clone();

    let id = fresh_request_id();
    let request = encode_stream_argv_with_id(&service_args, id).map_err(|e| e.to_string())?;
    let probe = mesh::mesh_api::envelope::decode(&request)
        .and_then(quic_tools::decode_probe_start_record);
    let default_timeout = match &probe {
        Some((_, p)) => Duration::from_secs(
            p.total_bytes()
                .saturating_add(8 * 1024 - 1)
                .div_euclid(8 * 1024)
                .max(30),
        ),
        None => Duration::from_secs(10),
    };
    let timeout = timeout_override.unwrap_or(default_timeout);
    let label = target.label();
    let bearer = target.bearer_name();

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        tokio::time::timeout(timeout, async {
            let (mux, _acceptor, mut serial_guard) =
                connect_mux_client(&target, TokioMuxConfig::default()).await?;
            let res = if let Some((probe_id, probe_req)) = probe {
                if probe_req.peer.is_some() {
                    Err("mux probe peer forwarding is not supported".into())
                } else {
                    match mux_probe(&mux, probe_req, probe_id).await {
                        Ok(result) => {
                            println!(
                                "dmesh_cli_probe bearer={bearer} target={label} direction={} bytes={} elapsed_us={} upload_mbps={:.3} download_mbps={:.3} upload_elapsed_us={} download_elapsed_us={} callback_errors={:?}",
                                probe_req.direction.as_str(),
                                result.bytes,
                                result.elapsed_us,
                                result.upload_mbps(),
                                result.download_mbps(),
                                result.upload_elapsed_us,
                                result.download_elapsed_us,
                                result.callback_errors,
                            );
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
            } else {
                let started = Instant::now();
                match mux_exchange(&mux, &request).await {
                    Ok(response) => {
                        println!(
                            "dmesh_cli_stream_command bearer={bearer} target={label} method={method_name} fin=true bytes={} elapsed_us={} {}",
                            response.len(),
                            started.elapsed().as_micros(),
                            render_stream_response(&request, &response),
                        );
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            };
            if serial_guard.is_some() {
                mux.send_connection_close(0);
                for _ in 0..50 {
                    if mux.is_flushed() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            mux.close();
            if let Some(mut g) = serial_guard.take() {
                if let Err(serial_err) = g.shutdown() {
                    return match res {
                        Err(err) => Err(format!("{err} (serial error: {serial_err})")),
                        Ok(()) => Err(serial_err),
                    };
                }
            }
            res
        })
        .await
        .map_err(|_| format!("mux stream timed out after {timeout:?}"))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        os::fd::{AsRawFd, FromRawFd},
    };

    fn open_pty_pair() -> (String, String, File, File) {
        let (mut master, mut slave) = (0, 0);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                )
            },
            0
        );
        for fd in [master, slave] {
            let mut tio: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut tio) }, 0);
            unsafe { libc::cfmakeraw(&mut tio) };
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) }, 0);
        }
        let master_file = unsafe { File::from_raw_fd(master) };
        let slave_file = unsafe { File::from_raw_fd(slave) };
        let master_path = std::fs::read_link(format!("/proc/self/fd/{}", master_file.as_raw_fd()))
            .unwrap()
            .display()
            .to_string();
        let slave_path = std::fs::read_link(format!("/proc/self/fd/{}", slave_file.as_raw_fd()))
            .unwrap()
            .display()
            .to_string();
        (master_path, slave_path, master_file, slave_file)
    }

    #[test]
    fn cli_mux_client_and_server_over_tcp_and_uds_all_directions() {
        let dir = tempfile::tempdir().unwrap();
        let uds_path = dir.path().join("mux.sock");

        let server_rt = tokio::runtime::Runtime::new().unwrap();
        let server = server_rt
            .block_on(start_mux_server(
                MuxServerConfig {
                    tcp: Some("127.0.0.1:0".parse().unwrap()),
                    uds: Some(uds_path.clone()),
                    ..Default::default()
                },
                default_mux_server_handler(),
            ))
            .unwrap();
        let tcp_addr = server.tcp_addr.unwrap();

        let _bg = std::thread::spawn(move || {
            server_rt.block_on(async move {
                let _ = server.join.await;
            });
        });

        // 1. Status RPC over TCP and UDS
        run_client_cli(&[format!("mux://{tcp_addr}"), "status".into()]).unwrap();
        run_client_cli(&[
            format!("mux+unix://{}", uds_path.display()),
            "status".into(),
        ])
        .unwrap();

        // 2. Validated streaming probe (upload, download, both) over TCP and UDS
        for target in [
            format!("mux://{tcp_addr}"),
            format!("mux+unix://{}", uds_path.display()),
        ] {
            for direction in ["upload", "download", "both"] {
                run_client_cli(&[
                    target.clone(),
                    "probe.start".into(),
                    "bytes=65537".into(),
                    "packet_size=1024".into(),
                    format!("direction={direction}"),
                    "verify=true".into(),
                    "--timeout-secs=15".into(),
                ])
                .unwrap();
            }
        }
    }

    // `openpty`'s master `/proc/self/fd/N` symlink resolves to `/dev/ptmx` (which opens a fresh
    // unrelated pty!), so bridge two real slave `/dev/pts/X` endpoints in-memory to emulate a
    // full-duplex UART cable with interleaved device boot text.
    fn spawn_pty_cable() -> (String, String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let (_, slave_a, mut master_a, keep_a) = open_pty_pair();
        let (_, slave_b, mut master_b, keep_b) = open_pty_pair();
        for f in [&master_a, &master_b] {
            unsafe {
                let flags = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(f.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let wire_stop = Arc::new(AtomicBool::new(false));
        let wire_stop_bg = wire_stop.clone();
        let wire = std::thread::spawn(move || {
            let _keep = (keep_a, keep_b);
            use std::collections::VecDeque;
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let mut a_to_b = VecDeque::<u8>::new();
            let mut b_to_a = VecDeque::<u8>::new();
            // Inject an ESP_LOG line before any PPP records to verify sideband log preservation.
            b_to_a.extend(b"I (120) main: mod_mux ready\r\n");
            while !wire_stop_bg.load(Ordering::Acquire) {
                let mut moved = false;
                if a_to_b.len() < 64 * 1024 {
                    if let Ok(n) = master_a.read(&mut buf) {
                        if n > 0 {
                            a_to_b.extend(&buf[..n]);
                            moved = true;
                        }
                    }
                }
                if b_to_a.len() < 64 * 1024 {
                    if let Ok(n) = master_b.read(&mut buf) {
                        if n > 0 {
                            b_to_a.extend(&buf[..n]);
                            moved = true;
                        }
                    }
                }
                if !a_to_b.is_empty() {
                    let (front, _) = a_to_b.as_slices();
                    match master_b.write(front) {
                        Ok(n) if n > 0 => {
                            a_to_b.drain(..n);
                            moved = true;
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => panic!("master_b write failed: {e}"),
                    }
                }
                if !b_to_a.is_empty() {
                    let (front, _) = b_to_a.as_slices();
                    match master_a.write(front) {
                        Ok(n) if n > 0 => {
                            b_to_a.drain(..n);
                            moved = true;
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => panic!("master_a write failed: {e}"),
                    }
                }
                if !moved {
                    std::thread::sleep(Duration::from_micros(250));
                }
            }
        });
        (slave_a, slave_b, wire_stop, wire)
    }

    #[test]
    fn cli_mux_over_ppp_serial_and_multipath_tcp_plus_serial() {

        // 1. Pure PPP serial (`mux+serial://`) bidirectional probe (also verifies `XipPort` Drop
        // clears `TIOCEXCL` so reopening the same `/dev/pts/X` for a second CLI invocation works!).
        let (slave_a, slave_b, wire_stop, wire) = spawn_pty_cable();
        let server_rt = tokio::runtime::Runtime::new().unwrap();
        let server = server_rt
            .block_on(start_mux_server(
                MuxServerConfig {
                    serial: Some(slave_b),
                    baud: Some(921600),
                    ..Default::default()
                },
                default_mux_server_handler(),
            ))
            .unwrap();
        let shutdown_tx = server.shutdown_tx.clone().unwrap();
        let server_thread = std::thread::spawn(move || {
            server_rt.block_on(async move { server.join.await.unwrap() })
        });

        run_client_cli(&[
            format!("mux+serial://{slave_a}"),
            "status".into(),
            "--baud".into(),
            "921600".into(),
        ])
        .unwrap();
        run_client_cli(&[
            format!("mux+serial://{slave_a}"),
            "probe.start".into(),
            "bytes=16385".into(),
            "packet_size=512".into(),
            "direction=both".into(),
            "verify=true".into(),
            "--baud".into(),
            "921600".into(),
            "--timeout-secs=20".into(),
        ])
        .unwrap();

        let _ = shutdown_tx.send(true);
        server_thread.join().unwrap().unwrap();
        wire_stop.store(true, Ordering::Release);
        wire.join().unwrap();

        // 2. Multipath TCP + PPP serial (`mux://HOST:PORT --serial /dev/pts/X`) into one server session
        let (slave_a2, slave_b2, wire_stop2, wire2) = spawn_pty_cable();
        let server_rt2 = tokio::runtime::Runtime::new().unwrap();
        let server2 = server_rt2
            .block_on(start_mux_server(
                MuxServerConfig {
                    tcp: Some("127.0.0.1:0".parse().unwrap()),
                    serial: Some(slave_b2),
                    baud: Some(921600),
                    ..Default::default()
                },
                default_mux_server_handler(),
            ))
            .unwrap();
        let tcp_addr = server2.tcp_addr.unwrap();
        let shutdown_tx2 = server2.shutdown_tx.clone().unwrap();
        let server_thread2 = std::thread::spawn(move || {
            server_rt2.block_on(async move { server2.join.await.unwrap() })
        });

        run_client_cli(&[
            format!("mux://{tcp_addr}"),
            "probe.start".into(),
            "bytes=32769".into(),
            "packet_size=1024".into(),
            "direction=both".into(),
            "verify=true".into(),
            "--serial".into(),
            slave_a2,
            "--baud".into(),
            "921600".into(),
            "--timeout-secs=20".into(),
        ])
        .unwrap();

        let _ = shutdown_tx2.send(true);
        server_thread2.join().unwrap().unwrap();
        wire_stop2.store(true, Ordering::Release);
        wire2.join().unwrap();
    }

    #[test]
    fn mesh_cli_cbor_stream_interoperates_with_dmesh_cli_mux_and_ppp() {
        let dir = tempfile::tempdir().unwrap();
        let uds_path = dir.path().join("mesh-interop.sock");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // 1. TCP & UDS (`TokioMux` per accepted client connection)
        let tcp_uds_server = rt
            .block_on(start_mux_server(
                MuxServerConfig {
                    tcp: Some("127.0.0.1:0".parse().unwrap()),
                    uds: Some(uds_path.clone()),
                    ..Default::default()
                },
                default_mux_server_handler(),
            ))
            .unwrap();
        let tcp_addr = tcp_uds_server.tcp_addr.unwrap();

        let check_client = |mux: TokioMux| async move {
            for req in [
                mesh::tagged::TaggedRecord {
                    component: mesh::tagged::NameOrTag::Tag(9),
                    method: mesh::tagged::NameOrTag::Tag(1),
                    id: Some(serde_json::json!(101)),
                    ..Default::default()
                },
                mesh::tagged::TaggedRecord {
                    component: mesh::tagged::NameOrTag::Name("diagnostic".to_owned()),
                    method: mesh::tagged::NameOrTag::Name("status".to_owned()),
                    id: Some(serde_json::json!(102)),
                    ..Default::default()
                },
            ] {
                let expected_id = req.id.clone();
                let frame = mux_exchange_cbor_stream(&mux, &req, &[]).await.unwrap();
                assert_eq!(frame.record.id, expected_id);
                assert_eq!(frame.record.result, Some(serde_json::json!({"1": "dmesh-cli"})));
                assert!(frame.body.is_empty());
            }
        };

        rt.block_on(async {
            for target in [
                MuxClientTarget {
                    tcp: Some(tcp_addr),
                    ..Default::default()
                },
                MuxClientTarget {
                    uds: Some(uds_path.clone()),
                    ..Default::default()
                },
            ] {
                let (mux, _acc, _) =
                    connect_mux_client(&target, TokioMuxConfig::default())
                        .await
                        .unwrap();
                check_client(mux.clone()).await;
                mux.close();
            }
        });
        rt.block_on(tcp_uds_server.stop()).unwrap();

        // 2. PPP serial (`PORT_MUX = 245`) over a PTY UART pair
        let (slave_a, slave_b, wire_stop, wire) = spawn_pty_cable();
        let ppp_server = rt
            .block_on(start_mux_server(
                MuxServerConfig {
                    serial: Some(slave_b),
                    baud: Some(921600),
                    ..Default::default()
                },
                default_mux_server_handler(),
            ))
            .unwrap();
        rt.block_on(async {
            let (mux, _acc, mut serial_guard) = connect_mux_client(
                &MuxClientTarget {
                    serial: Some(slave_a),
                    baud: Some(921600),
                    ..Default::default()
                },
                TokioMuxConfig::default(),
            )
            .await
            .unwrap();
            check_client(mux.clone()).await;
            mux.send_connection_close(0);
            for _ in 0..50 {
                if mux.is_flushed() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            mux.close();
            serial_guard.take().unwrap().shutdown().unwrap();
        });
        rt.block_on(ppp_server.stop()).unwrap();
        wire_stop.store(true, Ordering::Release);
        wire.join().unwrap();
    }
}
