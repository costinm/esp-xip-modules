//! Opt-in radio proof against two generic C Main devices with updated disc/nan.
//! XIP_DISCOVERY_SOURCE=SERIAL XIP_DISCOVERY_TARGET=SERIAL cargo test --test discovery_device -- --ignored --nocapture
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn command(device: &str, method: &str, fields: &[String]) -> Value {
    let read_only = fields.is_empty()
        && matches!(
            method,
            "console.status" | "nan.status" | "disc.routes" | "disc.routing" | "nan.datagrams"
        );
    for attempt in 0..2 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dmesh-cli"))
            .args(["console", device, method])
            .args(fields)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{device} {method}: CLI timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        if !output.status.success()
            && read_only
            && attempt == 0
            && (error.contains("did not reply") || error.contains("no awake acknowledgement"))
        {
            println!(
                "{}",
                json!({"event":"read_retry","device":device,"method":method,"error":error})
            );
            continue;
        }
        assert!(output.status.success(), "{device} {method}: {error}");
        let result = serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone();
        println!(
            "{}",
            json!({"event":"command","device":device,"method":method,"result":result})
        );
        return result;
    }
    unreachable!()
}

struct Cleanup(Vec<String>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for device in self.0.iter().rev() {
            let _ = Command::new(env!("CARGO_BIN_EXE_dmesh-cli"))
                .args(["console", device, "disc.routing", "--enabled=false"])
                .output();
        }
    }
}
#[test]
#[ignore = "requires two identified C Main devices, updated disc/nan and a shared radio channel"]
fn active_datagram_discovery_returns_a_correlated_receiver_route() {
    let source = std::env::var("XIP_DISCOVERY_SOURCE")
        .expect("select the source UART/USB or udp:// endpoint");
    let target = std::env::var("XIP_DISCOVERY_TARGET")
        .expect("select the target UART/USB or udp:// endpoint");
    assert_ne!(source, target);
    let mut cleanup = Cleanup(Vec::new());
    for device in [&source, &target] {
        let health = command(device, "console.status", &[]);
        assert!(health["4"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["1"] == "nan"));
        assert_eq!(
            command(device, "disc.routing", &[])["1"],
            false,
            "routing must initially be disabled"
        );
        cleanup.0.push(device.clone());
        assert_eq!(
            command(device, "disc.routing", &["--enabled=true".into()])["1"],
            true
        );
    }
    let target_mac = command(&target, "disc.routing", &[])["2"]
        .as_str()
        .unwrap()
        .to_owned();
    let admitted = command(
        &source,
        "disc.discover",
        &[format!("--target={target_mac}"), "--hop_limit=2".into()],
    );
    let deadline = Instant::now() + Duration::from_millis(admitted["3"].as_u64().unwrap());
    loop {
        let routes = command(&source, "disc.routes", &[]);
        if let Some(route) =
            routes["1"].as_array().unwrap().iter().find(|r| {
                r["1"] == target_mac && r["8"] == admitted["1"] && r["9"] == admitted["2"]
            })
        {
            assert!((1..=2).contains(&route["3"].as_u64().unwrap()));
            assert!(
                (-128..=0).contains(&route["6"].as_i64().unwrap()),
                "signed RSSI or native unknown=0"
            );
            assert!(
                command(&target, "disc.routing", &[])["3"].as_u64().unwrap() > 0,
                "target must admit the request"
            );
            println!("correlated receiver route: {route}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "NAN admission did not establish a target response before timeout"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[derive(Deserialize)]
struct Device {
    name: String,
    endpoint: String,
    mac: String,
    #[serde(default)]
    nan_version: Option<u64>,
    #[serde(default)]
    disc_version: Option<u64>,
}
#[derive(Deserialize)]
struct Matrix {
    version: u64,
    devices: Vec<Device>,
    #[serde(default)]
    case_limit: Option<usize>,
}
fn mac(value: &str) -> String {
    value
        .trim_start_matches("hex:")
        .replace(':', "")
        .to_ascii_lowercase()
}

/// Each device originates once and is a target once. Batches respect the
/// four-new-requests/minute admission limit, including overheard floods.
#[test]
#[ignore = "requires XIP_DISCOVERY_MATRIX with at least three updated C Main devices"]
fn fleet_active_discovery_has_receiver_proof_and_bounded_holds() {
    let path =
        std::env::var("XIP_DISCOVERY_MATRIX").expect("select a discovered device matrix JSON");
    let matrix: Matrix = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(matrix.devices.len() >= 3);
    let mut cleanup = Cleanup(Vec::new());
    let mut channel = None;
    for (index, device) in matrix.devices.iter().enumerate() {
        assert!(!matrix.devices[..index]
            .iter()
            .any(|other| other.endpoint == device.endpoint || mac(&other.mac) == mac(&device.mac)));
        let health = command(&device.endpoint, "console.status", &[]);
        assert_eq!(health["1"]["4"], "main");
        assert_eq!(mac(health["1"]["7"].as_str().unwrap()), mac(&device.mac));
        for name in ["disc", "nan"] {
            let module = health["4"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["1"] == name)
                .unwrap();
            let version = if name == "nan" {
                device.nan_version.unwrap_or(matrix.version)
            } else {
                device.disc_version.unwrap_or(matrix.version)
            };
            assert_eq!(
                module["5"].as_u64(),
                Some(version),
                "{} {name} version",
                device.name
            );
        }
        let actual = health["2"]["6"].as_u64().unwrap();
        if let Some(channel) = channel {
            assert_eq!(actual, channel, "test endpoints must share a radio channel");
        }
        channel = Some(actual);
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let nan = command(&device.endpoint, "nan.status", &[]);
            if nan["4"].as_u64().is_some_and(|tsf| tsf > 0) && nan["2"] != "hex:000000000000" {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "{} has no radio clock",
                device.name
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        cleanup.0.push(device.endpoint.clone());
        command(
            &device.endpoint,
            "disc.routing",
            &["--enabled=false".into()],
        );
        let enabled = command(&device.endpoint, "disc.routing", &["--enabled=true".into()]);
        assert_eq!(enabled["1"], true);
        assert_eq!(mac(enabled["2"].as_str().unwrap()), mac(&device.mac));
    }
    let mut batch_start = Instant::now();
    for (index, source) in matrix
        .devices
        .iter()
        .enumerate()
        .take(matrix.case_limit.unwrap_or(matrix.devices.len()))
    {
        if index > 0 && index % 4 == 0 {
            while batch_start.elapsed() < Duration::from_secs(61) {
                std::thread::sleep(Duration::from_millis(250));
            }
            batch_start = Instant::now();
        }
        let target = &matrix.devices[(index + 1) % matrix.devices.len()];
        let before = command(&target.endpoint, "disc.routing", &[]);
        let hold_before = command(&target.endpoint, "nan.awake", &["--duration_ms=0".into()])["1"]
            .as_u64()
            .unwrap();
        let started = Instant::now();
        let admission = command(
            &source.endpoint,
            "disc.discover",
            &[
                format!("--target=hex:{}", mac(&target.mac)),
                "--hop_limit=3".into(),
            ],
        );
        let timeout = Duration::from_millis(admission["3"].as_u64().unwrap());
        let route = loop {
            let result = command(&source.endpoint, "disc.routes", &[]);
            if let Some(route) = result["1"].as_array().unwrap().iter().find(|r| {
                mac(r["1"].as_str().unwrap()) == mac(&target.mac)
                    && r["8"] == admission["1"]
                    && r["9"] == admission["2"]
            }) {
                break route.clone();
            }
            assert!(
                started.elapsed() < timeout,
                "{} -> {}: no correlated target reply",
                source.name,
                target.name
            );
            std::thread::sleep(Duration::from_millis(250));
        };
        assert!((1..=3).contains(&route["3"].as_u64().unwrap()));
        assert!((-128..=0).contains(&route["6"].as_i64().unwrap()));
        let after = command(&target.endpoint, "disc.routing", &[]);
        assert!(
            after["3"].as_u64().unwrap() > before["3"].as_u64().unwrap(),
            "target-side request admission"
        );
        // Let both DW0/DW8 copies run. Querying an activity window does not
        // renew the independent awake_for vote; source-side polling is separate.
        // A sleepy/capture-gated receiver can first admit near the final
        // rendezvous. Its four seconds begin there, not at origin admission.
        let hold_deadline = Instant::now() + Duration::from_millis(4_500);
        while started.elapsed() < Duration::from_millis(10_500)
            || Instant::now() < hold_deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let schedules = command(&source.endpoint, "nan.datagrams", &[]);
        assert!(
            schedules["3"].as_u64().unwrap() > 0,
            "all scheduled copies admitted"
        );
        assert_eq!(
            schedules["4"].as_u64(),
            Some(0),
            "a required rendezvous phase expired"
        );
        assert!(
            schedules["6"].as_u64().unwrap() > 0,
            "native submissions, independent of receiver proof"
        );
        let hold_after = command(&target.endpoint, "nan.awake", &["--duration_ms=0".into()])["1"]
            .as_u64()
            .unwrap();
        if hold_before == 0 {
            assert_eq!(
                hold_after, 0,
                "duplicate requests must not renew the four-second hold"
            );
        }
        println!(
            "{}",
            json!({"event":"route_verified","source":source.name,"target":target.name,
            "route":route,"elapsed_ms":started.elapsed().as_millis(),"hold_before_ms":hold_before,"hold_after_ms":hold_after,
            "hold_expiry_checked":hold_before==0,"schedules":schedules})
        );
    }
}

struct RestoreBoot(String);
impl Drop for RestoreBoot {
    fn drop(&mut self) {
        let result = Command::new(env!("CARGO_BIN_EXE_dmesh-cli"))
            .args([&self.0, "--reset"])
            .output();
        println!(
            "{}",
            json!({"event":"restore_boot_policy","device":self.0,
            "reset_accepted":result.as_ref().is_ok_and(|r|r.status.success())})
        );
    }
}

#[test]
#[ignore = "requires updated source and physical-UART ESP32 target; temporarily selects sleepy cadence 8, restores by reset"]
fn sleepy_target_answers_without_uart_wake_and_returns_to_sleep() {
    let source = std::env::var("XIP_DISCOVERY_SOURCE").unwrap();
    let target = std::env::var("XIP_DISCOVERY_TARGET").unwrap();
    assert_ne!(source, target);
    assert!(target.starts_with('/'), "physical UART target required");
    let health = command(&target, "console.status", &[]);
    assert_eq!(
        health["1"]["2"], 1,
        "classic ESP32 has no native USB sleep veto"
    );
    let restore = RestoreBoot(target.clone());
    let mut cleanup = Cleanup(Vec::new());
    for device in [&source, &target] {
        cleanup.0.push(device.clone());
        command(device, "disc.routing", &["--enabled=false".into()]);
        command(device, "disc.routing", &["--enabled=true".into()]);
    }
    let target_mac = command(&target, "disc.routing", &[])["2"]
        .as_str()
        .unwrap()
        .to_owned();
    let power_before = command(&target, "nan.powerinfo", &[]);
    command(
        &target,
        "nan.sleepy",
        &[
            "--enabled=true".into(),
            "--cadence=8".into(),
            "--immediate=true".into(),
        ],
    );
    // From this point until the correlated reply, all control traffic goes to
    // the origin. Place immediate NOW well outside the target's DW0/DW8 pair.
    std::thread::sleep(Duration::from_secs(3));
    let clock = command(&source, "nan.status", &[]);
    let current = command(&source, "console.status", &[])["1"]["1"]
        .as_u64()
        .unwrap()
        * 1000;
    let tsf = clock["4"].as_u64().unwrap() + current.saturating_sub(clock["5"].as_u64().unwrap());
    let period = 8 * 512 * 1024u64;
    let delay = (2_000_000 + period - tsf % period) % period;
    std::thread::sleep(Duration::from_micros(delay));
    let started = Instant::now();
    let admission = command(
        &source,
        "disc.discover",
        &[format!("--target={target_mac}"), "--hop_limit=3".into()],
    );
    let route =
        loop {
            let result = command(&source, "disc.routes", &[]);
            if let Some(route) = result["1"].as_array().unwrap().iter().find(|r| {
                r["1"] == target_mac && r["8"] == admission["1"] && r["9"] == admission["2"]
            }) {
                break route.clone();
            }
            if started.elapsed() >= Duration::from_millis(admission["3"].as_u64().unwrap()) {
                // Failure diagnostics may wake the target; they cannot count as
                // receiver proof. Capture them before reset discards evidence.
                let schedules = command(&source, "nan.datagrams", &[]);
                let receiver = command(&target, "disc.routing", &[]);
                let power = command(&target, "nan.powerinfo", &[]);
                let timing = command(&target, "nan.status", &[]);
                println!("{}", json!({"event":"sleepy_timeout_diagnostics",
                    "schedules":schedules,"receiver":receiver,"power":power,
                    "timing":timing,"power_before":power_before}));
                panic!("sleepy target did not answer");
            }
            std::thread::sleep(Duration::from_millis(250));
        };
    while started.elapsed() < Duration::from_millis(12_000) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let receiver = command(&target, "disc.routing", &[]);
    let hold = command(&target, "nan.awake", &["--duration_ms=0".into()]);
    let power_after = command(&target, "nan.powerinfo", &[]);
    assert!(receiver["3"].as_u64().unwrap() > 0);
    assert!(
        power_after["13"].as_u64().unwrap() > power_before["13"].as_u64().unwrap(),
        "physical sleeps must have occurred"
    );
    assert_eq!(power_after["25"], 0, "physical, not USB soft sleep");
    assert_eq!(
        hold["1"], 0,
        "receiver work hold expires despite repeated copies"
    );
    println!(
        "{}",
        json!({"event":"sleepy_route_verified","route":route,"elapsed_ms":started.elapsed().as_millis(),
        "clock_phase_at_immediate_us":(tsf+delay)%period,"receiver":receiver,"power_before":power_before,"power_after":power_after,"hold":hold})
    );
    drop(cleanup);
    drop(restore);
    std::thread::sleep(Duration::from_secs(2));
    let restored = command(&target, "console.status", &[]);
    assert_eq!(restored["1"]["7"], health["1"]["7"]);
    assert!(
        restored["1"]["1"].as_u64().unwrap() < 30_000,
        "fresh post-reset health restores boot policy"
    );
}
