//! Opt-in C Main test: demand activation and allocated bidirectional circuits.
//! Run: python3 tests/e2e/run.py run relay (see tests/e2e/scenarios/relay.md).
use serde_json::Value;
use std::process::Command;
fn cli(args: &[&str]) -> Result<String, String> {
    let out = Command::new(std::env::var("DMESH_CLI").unwrap_or_else(|_| env!("CARGO_BIN_EXE_dmesh-cli").to_owned()))
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::json!({"argv":args,"stdout":String::from_utf8_lossy(&out.stdout),"stderr":String::from_utf8_lossy(&out.stderr),"success":out.status.success()}));
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}
fn record(args: &[&str]) -> Value {
    serde_json::from_str(&cli(args).unwrap()).unwrap()
}
fn create(
    device: &str,
    transport: &str,
    dest: &str,
    rewrite: &str,
    endpoint: bool,
    idle_ms: u32,
    reuse: bool,
) -> Value {
    record(&[
        device,
        "disc.relay_create",
        &format!("--transport={transport}"),
        &format!("--dest={dest}"),
        &format!("--rewritten_dcid={rewrite}"),
        &format!("--endpoint={endpoint}"),
        &format!("--idle_ms={idle_ms}"),
        &format!("--reuse={reuse}"),
    ])
}
struct Cleanup {
    device: String,
    started: Vec<&'static str>,
    routes: Vec<String>,
}
impl Cleanup {
    fn create(
        &mut self,
        transport: &str,
        dest: &str,
        rewrite: &str,
        endpoint: bool,
        idle: u32,
    ) -> String {
        let v = create(
            &self.device,
            transport,
            dest,
            rewrite,
            endpoint,
            idle,
            false,
        );
        assert_eq!(v["result"]["7"], false);
        let cid = v["result"]["1"].as_str().unwrap().to_owned();
        assert!(!self.routes.contains(&cid));
        self.routes.push(cid.clone());
        cid
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for cid in &self.routes {
            let _ = cli(&[&self.device, "disc.relay_remove", &format!("--dcid={cid}")]);
        }
        for name in self.started.iter().rev() {
            let _ = cli(&[&self.device, "console.stop", &format!("--name={name}")]);
        }
    }
}
fn topology(
    device: &str,
    method: &str,
    cid: &str,
    transport: &str,
    dest: &str,
    rewrite: &str,
    weight: Option<u16>,
) -> Value {
    let owned = vec![
        format!("--dcid={cid}"),
        format!("--transport={transport}"),
        format!("--dest={dest}"),
        format!("--rewritten_dcid={rewrite}"),
    ];
    let mut args = vec![device, method];
    args.extend(owned.iter().map(String::as_str));
    let w = weight.map(|n| format!("--weight={n}"));
    if let Some(w) = &w {
        args.push(w)
    }
    record(&args)
}
#[test]
#[ignore = "requires updated C Main/disc/test, UART and STA UDP; disc/quic/test stopped"]
fn demand_activation_allocated_topology_activity_expiry_and_both_first_hops() {
    let device = std::env::var("XIP_RELAY_DEVICE").expect("select a discovered UART path");
    assert!(device.starts_with('/'));
    let before = record(&[&device, "console.status"]);
    let modules = before["result"]["4"].as_array().unwrap();
    for name in ["disc", "quic", "test"] {
        assert!(
            modules.iter().any(|m| m["1"] == name && m["4"] == false),
            "stop unrelated {name} work before this test"
        );
    }
    let ip = before["result"]["2"]["3"]
        .as_str()
        .expect("STA must have an IP address");
    let udp = format!("udp://{ip}:3339");
    let mut cleanup = Cleanup {
        device: device.clone(),
        started: vec!["disc", "quic", "test"],
        routes: Vec::new(),
    };
    // Build from the return end. Every intermediate uses the DCID returned by
    // the next device; none of the incoming IDs is chosen by the caller.
    let r5 = cleanup.create("serial", "247", "01", false, 60000);
    let r4 = cleanup.create("udp", &udp, &r5, false, 60000);
    let endpoint = cleanup.create("udp", &udp, &r4, true, 60000);
    let r2 = cleanup.create("udp", &udp, &endpoint, false, 60000);
    let r1 = cleanup.create("udp", &udp, &r2, false, 60000);
    let reused = create(&device, "udp", &udp, &r2, false, 60000, true);
    assert_eq!(reused["result"]["1"], r1);
    assert_eq!(reused["result"]["7"], true);
    let extra = cleanup.create("udp", &udp, &endpoint, false, 60000);
    topology(&device, "disc.relay_add", &r1, "udp", &udp, &extra, Some(2));
    let get = || record(&[&device, "disc.relay_get", &format!("--dcid={r1}")]);
    assert_eq!(get()["result"]["2"].as_array().unwrap().len(), 2);
    topology(
        &device,
        "disc.relay_remove_hop",
        &r1,
        "udp",
        &udp,
        &extra,
        None,
    );
    assert_eq!(get()["result"]["2"].as_array().unwrap().len(), 1);
    topology(&device, "disc.relay_add", &r1, "udp", &udp, &extra, Some(2));
    let via = format!("{device}:{r1}");
    let response = record(&[&device, "console.status", "--via", &via]);
    assert_eq!(response["result"]["1"]["7"], before["result"]["1"]["7"]);
    // Packet commands did not need the QUIC endpoint. Its first handshake and
    // the probe's stream opening must activate both owners automatically.
    let cold = record(&[&device, "console.status"]);
    for name in ["quic", "test"] {
        assert!(cold["result"]["4"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["1"] == name && m["4"] == false));
    }
    let used_before = get()["result"]["5"].as_u64().unwrap();
    for _ in 0..3 {
        let result = cli(&[
            "probe",
            &device,
            "--via",
            &via,
            "--bytes=65536",
            "--packet_size=512",
            "--direction=both",
            "--verify=true",
        ])
        .unwrap();
        assert!(
            result.contains("bytes=131072")
                && result.contains("callback_errors=[0, 0, 0, 0, 0, 0]"),
            "{result}"
        );
    }
    assert!(get()["result"]["5"].as_u64().unwrap() > used_before);
    let remote: std::net::IpAddr = ip.parse().unwrap();
    let discovery = std::net::UdpSocket::bind(if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .unwrap();
    discovery
        .connect(std::net::SocketAddr::new(remote, 3339))
        .unwrap();
    let reply = format!(
        "udp://{}",
        std::net::SocketAddr::new(discovery.local_addr().unwrap().ip(), 3338)
    );
    topology(&device, "disc.relay_add", &r5, "udp", &reply, "01", Some(1));
    topology(
        &device,
        "disc.relay_remove_hop",
        &r5,
        "serial",
        "247",
        "01",
        None,
    );
    let via_udp = format!("{ip}:{r1}");
    let response = record(&[&device, "console.status", "--via", &via_udp]);
    assert_eq!(response["result"]["1"]["7"], before["result"]["1"]["7"]);
    let result = cli(&[
        "probe",
        "tcp://127.0.0.1:5228",
        "--via",
        &via_udp,
        "--bytes=65536",
        "--packet_size=512",
        "--direction=both",
        "--verify=true",
    ])
    .unwrap();
    assert!(
        result.contains("bytes=131072") && result.contains("callback_errors=[0, 0, 0, 0, 0, 0]"),
        "{result}"
    );
    for cid in &cleanup.routes {
        record(&[&device, "disc.relay_remove", &format!("--dcid={cid}")]);
        let error = cli(&[&device, "disc.relay_get", &format!("--dcid={cid}")]).unwrap_err();
        assert!(error.contains("-7"), "{error}");
    }
    cleanup.routes.clear();
    // Control reads do not renew the lease. Expiry also releases the last
    // destination reference, and a fresh create cannot recycle the old DCID.
    let expiring = cleanup.create("serial", "247", "01", false, 100);
    let observed = record(&[&device, "disc.relay_get", &format!("--dcid={expiring}")]);
    let last = observed["result"]["5"].as_u64().unwrap();
    let observed = record(&[&device, "disc.relay_get", &format!("--dcid={expiring}")]);
    assert_eq!(observed["result"]["5"], last);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let error = cli(&[&device, "disc.relay_get", &format!("--dcid={expiring}")]).unwrap_err();
    assert!(error.contains("-7"), "{error}");
    cleanup.routes.clear();
    let fresh = cleanup.create("serial", "247", "01", false, 0);
    assert_ne!(fresh, expiring);
    record(&[
        &device,
        "disc.relay_set_idle",
        &format!("--dcid={fresh}"),
        "--idle_ms=100",
    ]);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        cli(&[&device, "disc.relay_get", &format!("--dcid={fresh}")])
            .unwrap_err()
            .contains("-7")
    );
    cleanup.routes.clear();
    let stopped = cleanup.started.clone();
    drop(cleanup);
    let after = record(&[&device, "console.status"]);
    for name in stopped {
        assert!(
            after["result"]["4"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["1"] == name && m["4"] == false),
            "{name} did not stop"
        );
    }
}
