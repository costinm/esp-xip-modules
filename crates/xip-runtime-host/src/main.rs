//! Run host modules with their declared TCP/UDP ports and an optional start request.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use xip_runtime_host::Runtime;

/// XIP_THREADS=1 runs everything on one thread like the device runtimes; otherwise
/// tokio's worker threads serve socket I/O and timers while module handlers stay
/// serialized on the thread that drives the runtime.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = match std::env::var("XIP_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
        Some(1) => tokio::runtime::Builder::new_current_thread(),
        Some(n) if n > 1 => {
            let mut b = tokio::runtime::Builder::new_multi_thread();
            b.worker_threads(n);
            b
        }
        _ => tokio::runtime::Builder::new_multi_thread(),
    };
    b.enable_all().build()?.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut specs: Vec<_> = std::env::args().skip(1).collect();
    if specs.iter().any(|s| s == "--help" || s == "-h") {
        println!("usage: xip-host [PATH.wasm[=START] ...]\nXIP_THREADS=1 selects the single-thread mode (as on the device)\nXIP_BASE_PORT=N offsets the manifest ports (default PORT_NET), XIP_BIND=IP the bind address (default 127.0.0.1)\nXIP_STATE_DIR=DIR keeps the device identity across runs\nXIP_QUIC_PORT=N the UDP port of QUIC packets (default 3340, 0 any, off none)\nDefault: mod_con (no port, requests arrive as QUIC packets on the UDP port) and an async echo on TCP/UDP 5228");
        return Ok(());
    }
    if specs.is_empty() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules");
        specs = vec![root.join("mod_con.wasm").display().to_string(), format!("{}=async", root.join("mod_rtest.wasm").display())];
    }
    // Manifest ports are offsets from the base, so several instances can share a machine.
    let base: u16 = std::env::var("XIP_BASE_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(xip_runtime_host::xip_abi::PORT_NET as u16);
    let bind: IpAddr = std::env::var("XIP_BIND").ok().and_then(|v| v.parse().ok()).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    // The device identity (P-256) persists in XIP_STATE_DIR/identity.p256; without it a new identity is made at every start.
    if let Some(dir) = std::env::var_os("XIP_STATE_DIR") {
        xip_runtime_host::set_identity_dir(dir)?;
    }
    let rt = Runtime::default();
    let mut modules = Vec::new();
    for spec in &specs {
        let (path, request) = spec.split_once('=').unwrap_or((spec, ""));
        let m = rt.load_module(path).map_err(std::io::Error::other)?;
        let ports = m.listen_network(bind, base).await?;
        eprintln!("{path}: TCP {:?}, UDP {:?}", ports.stream, ports.datagram);
        m.start(request.as_bytes()); modules.push(m);
    }
    // QUIC datagrams (dmesh-cli, other nodes): one UDP link in front of the classifier; XIP_QUIC_PORT=N (default 3340, 0: any, off: none)
    let mut quic = match std::env::var("XIP_QUIC_PORT").as_deref() {
        Ok("off") => None,
        v => {
            let explicit = v.is_ok();
            let port = v.ok().and_then(|v| v.parse().ok()).unwrap_or(xip_runtime_host::QUIC_UDP_PORT);
            match xip_runtime_host::QuicUdp::bind(&rt, SocketAddr::new(bind, port)).await {
                Ok(q) => { eprintln!("QUIC: UDP {}", q.local_addr()?); Some(q) }
                // The default port may belong to another process on this machine: carry on without QUIC.
                Err(e) if !explicit => { eprintln!("QUIC: UDP {port} unavailable ({e}); set XIP_QUIC_PORT"); None }
                Err(e) => return Err(e.into()),
            }
        }
    };
    loop {
        if let Some(q) = quic.as_mut() { q.flush(&rt).await; }
        tokio::select! {
            r = async { match quic.as_mut() { Some(q) => q.recv(&rt).await, None => std::future::pending().await } } => { r?; },
            _ = rt.step_realtime(Duration::from_secs(86400)) => {},
            _ = tokio::signal::ctrl_c() => { break; },
        }
    }
    for m in modules { m.stop(); }
    rt.run(0);
    Ok(())
}
