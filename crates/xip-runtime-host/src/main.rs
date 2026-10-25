//! Run host modules with their declared TCP/UDP ports and an optional start request.
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use xip_runtime_host::Runtime;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut specs: Vec<_> = std::env::args().skip(1).collect();
    if specs.iter().any(|s| s == "--help" || s == "-h") {
        println!("usage: xip-host [PATH.so[=START] ...]\nDefault: CBOR console on loopback UDP 5226; async echo on TCP/UDP 5228");
        return Ok(());
    }
    if specs.is_empty() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules");
        specs = vec![root.join("mod_con.so").display().to_string(), format!("{}=async", root.join("mod_rtest.so").display())];
    }
    let rt = Runtime::default();
    let mut modules = Vec::new();
    for spec in &specs {
        let (path, request) = spec.split_once('=').unwrap_or((spec, ""));
        let m = rt.load_module(path).map_err(std::io::Error::other)?;
        let ports = m.listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), xip_runtime_host::xip_abi::PORT_NET as u16).await?;
        eprintln!("{path}: TCP {:?}, UDP {:?}", ports.stream, ports.datagram);
        m.start(request.as_bytes()); modules.push(m);
    }
    loop {
        tokio::select! {
            _ = rt.step_realtime(Duration::from_secs(86400)) => {},
            _ = tokio::signal::ctrl_c() => { break; },
        }
    }
    for m in modules { m.stop(); }
    rt.run(0);
    Ok(())
}
