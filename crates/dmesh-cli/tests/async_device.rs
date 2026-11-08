//! Binary PPP echo and burst admission through the CLI library's only UART owner.
//! Selected by tests/e2e/run.py; see tests/e2e/scenarios/async-io.md.
use dmesh_cli::xip_wire::{
    Event, PORT_QUIC, Reader, XipPort, encode_record, unwrap_answer, wrap_request,
};
use std::{
    collections::BTreeSet,
    process::Command,
    time::{Duration, Instant},
};

fn cli(device: &str, words: &[&str]) {
    let bin =
        std::env::var("DMESH_CLI").unwrap_or_else(|_| env!("CARGO_BIN_EXE_dmesh-cli").to_owned());
    let output = Command::new(bin).arg(device).args(words).output().unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn start(device: &str, mode: &str) {
    let request = format!(
        "--request=hex:{}",
        mode.bytes().map(|b| format!("{b:02x}")).collect::<String>()
    );
    cli(device, &["console.start", "--name=test", &request]);
}
struct Stop(String);
impl Drop for Stop {
    fn drop(&mut self) {
        let bin = std::env::var("DMESH_CLI")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_dmesh-cli").to_owned());
        let _ = Command::new(bin)
            .args([&self.0, "console.stop", "--name=test"])
            .output();
    }
}
fn echo_request(id: u64) -> Vec<u8> {
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(4)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(2)
        .unwrap()
        .u8(2)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(3)
        .unwrap()
        .u64(id)
        .unwrap()
        .u8(5)
        .unwrap()
        .map(1)
        .unwrap()
        .u8(1)
        .unwrap()
        .str(&format!("burst-{id}"))
        .unwrap();
    encode_record(PORT_QUIC, &wrap_request(&e.into_writer()))
}
fn answer(bytes: &[u8]) -> Option<(u64, Result<String, i32>)> {
    let bytes = unwrap_answer(bytes)?;
    let mut d = minicbor::Decoder::new(bytes);
    let n = d.map().ok()??;
    let (mut component, mut method, mut id, mut result) = (None, None, None, None);
    for _ in 0..n {
        match d.u8().ok()? {
            1 => component = Some(d.u64().ok()?),
            2 => method = Some(d.u64().ok()?),
            3 => id = Some(d.u64().ok()?),
            6 => {
                if d.map().ok()?? != 1 || d.u8().ok()? != 1 {
                    return None;
                }
                result = Some(Ok(d.str().ok()?.to_owned()));
            }
            7 => result = Some(Err(d.i32().ok()?)),
            _ => {
                d.skip().ok()?;
            }
        }
    }
    if component != Some(2) || method != Some(1) || d.position() != bytes.len() {
        return None;
    }
    Some((id?, result?))
}
#[test]
#[ignore = "use the discovery-based E2E runner; needs stopped test module and exclusive UART"]
fn binary_echo_and_burst_busy_recover_without_losing_requests() {
    let device = std::env::var("XIP_ASYNC_DEVICE")
        .expect("select a discovered UART through tests/e2e/run.py");
    let stop = Stop(device.clone());
    start(&device, "async-pause");
    {
        // Uses the existing CLI port/framing library, no second UART codec.
        let mut owner = XipPort::open(device.clone()).unwrap();
        owner.wake().unwrap();
        let burst: Vec<u8> = (1..=12).flat_map(echo_request).collect();
        owner.write_all(&burst).unwrap();
        let mut reader = Reader::default();
        let mut seen = BTreeSet::new();
        let (mut busy, mut admitted) = (0, 0);
        let deadline = Instant::now() + Duration::from_secs(8);
        while seen.len() < 12 && Instant::now() < deadline {
            for event in reader.feed(&owner.read().unwrap()) {
                if let Event::Record(PORT_QUIC, bytes) = event {
                    if let Some((id, result)) = answer(&bytes) {
                        assert!(
                            (1..=12).contains(&id) && seen.insert(id),
                            "unexpected/duplicate request id {id}"
                        );
                        match result {
                            Ok(value) => {
                                assert_eq!(value, format!("burst-{id}"));
                                admitted += 1;
                            }
                            Err(-5) => busy += 1,
                            Err(error) => panic!("unexpected admission error {error}"),
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            seen.len(),
            12,
            "every request must complete or explicitly return BUSY"
        );
        assert!(
            busy > 0 && admitted > 0,
            "need both refusal and admitted payload evidence"
        );
        owner.write_all(&echo_request(100)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut recovered = false;
        while !recovered && Instant::now() < deadline {
            for event in reader.feed(&owner.read().unwrap()) {
                if let Event::Record(PORT_QUIC, bytes) = event {
                    if let Some((100, result)) = answer(&bytes) {
                        assert_eq!(result.unwrap(), "burst-100");
                        recovered = true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(recovered, "async owner did not recover after BUSY");
        let payload: Vec<u8> = [0, 0x7e, 0x7d, 0xff]
            .into_iter()
            .chain((0..=255).cycle().take(512))
            .collect();
        owner.write_all(&encode_record(3, &payload)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut echoed = false;
        while !echoed && Instant::now() < deadline {
            for event in reader.feed(&owner.read().unwrap()) {
                if let Event::Record(3, bytes) = event {
                    assert_eq!(bytes, payload);
                    echoed = true;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(echoed, "binary PPP datagram echo missing");
        println!(
            "async_io admitted={admitted} busy={busy} completed=12 recovered=true binary_bytes={}",
            payload.len()
        );
    }
    drop(stop);
}
