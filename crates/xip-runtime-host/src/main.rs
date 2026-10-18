//! Run host modules with their declared TCP/UDP ports and an optional start request.
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use xip_runtime_host::{NativeTx, Runtime};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut specs: Vec<_> = std::env::args().skip(1).collect();
    if specs.iter().any(|s| s == "--help" || s == "-h") {
        println!("usage: xip-host [PATH.so[=START] ...]\nDefault: console + async test echo on loopback TCP/UDP 5226 and 5228");
        return Ok(());
    }
    if specs.is_empty() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules");
        specs = vec![root.join("mod_con.so").display().to_string(), format!("{}=async", root.join("mod_rtest.so").display())];
    }
    let rt = Runtime::default();
    rt.add_native_link("serial", 256, NativeTx::Done);
    let mut modules = Vec::new();
    for spec in &specs {
        let (path, request) = spec.split_once('=').unwrap_or((spec, ""));
        let m = rt.load_module(path).map_err(std::io::Error::other)?;
        let ports = m.listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), xip_runtime_host::xip_abi::PORT_NET as u16).await?;
        eprintln!("{path}: TCP {:?}, UDP {:?}", ports.stream, ports.datagram);
        m.start(request.as_bytes()); modules.push(m);
    }
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) { if tx.send(line).is_err() { break; } }
    });
    let mut stdin_open = true;
    loop {
        for (_, frame) in rt.take_link_tx("serial") { std::io::stdout().write_all(&frame)?; }
        std::io::stdout().flush()?;
        // A socket event or timer drives the dispatcher; stdin wakes this select.
        tokio::select! {
            _ = rt.step_realtime(Duration::from_secs(86400)) => {},
            line = lines.recv(), if stdin_open => {
                if let Some(line) = line { rt.inject_frame("serial", 0, 0, line.as_bytes()); rt.run(0); }
                else { stdin_open = false; }
            },
            _ = tokio::signal::ctrl_c() => { break; },
        }
    }
    for m in modules { m.stop(); }
    rt.run(0);
    Ok(())
}
