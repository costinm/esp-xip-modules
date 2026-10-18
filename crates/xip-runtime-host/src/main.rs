//! `xip-host`: run modules on the development machine, the host twin of `main/` and `main-rs/`.
//!
//!   xip-host [--ms N] PATH.so:COMPONENT[:console] ...
//!
//! Loads each module (built by `scripts/build-host-modules.sh`: `target/host-modules/mod_con.so`), gives it its component number, starts it, and
//! runs the event loop in real time. A module marked `console` is bound to the console port (datagram port 1): the lines typed on stdin are
//! frames of the `serial` link and what the modules send back to it is printed on stdout. Alarms, packets and messages work as on a device;
//! the host calls (wifi, gpio, ...) are the fixtures of the library.
//!
//! TODO: feature parity with the Rust runtime (`xip-runtime-esp`): a module should be testable functionally on the host, with the same behaviour
//! as on a device. Reuse the code of `xip-runtime-esp` (the PPP decoder, the wifi store, the ESP-NOW frame codec, the loader logic) and replace only what
//! touches hardware: tokio or fakes for sockets and timers, stubbed, faked or pcap-driven backends for the radios (Wi-Fi, ESP-NOW, LoRa), UART, gpio and flash.
//!
//! TODO: sockets and datagram listeners (the manifest's stream and datagram ports on TCP and UDP of this machine), the Rust receive path
//! shared with `xip-runtime-esp`, and `.dmod` images instead of shared objects.
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::Duration;
use xip_runtime_host::{NativeTx, Runtime};

fn main() {
    let mut tick = 10u32;
    let mut specs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--ms" {
            tick = args.next().and_then(|v| v.parse().ok()).unwrap_or(10);
        } else {
            specs.push(a);
        }
    }
    if specs.is_empty() {
        eprintln!("usage: xip-host [--ms N] PATH.so:COMPONENT[:console] ...");
        std::process::exit(2);
    }
    let rt = Runtime::default();
    rt.add_native_link("serial", 256, NativeTx::Done);
    let mut modules = Vec::new();
    for spec in &specs {
        let mut parts = spec.split(':');
        let path = parts.next().unwrap_or_default();
        let Some(component) = parts.next().and_then(|c| c.parse::<i32>().ok()) else {
            eprintln!("{spec}: expected PATH:COMPONENT");
            std::process::exit(2);
        };
        let console = parts.next() == Some("console");
        let m = match rt.add_module(path, 256) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{path}: {e}");
                std::process::exit(1);
            }
        };
        m.set_component(component);
        if console {
            m.bind_port(xip_abi::PORT_CONSOLE);
        }
        m.start(b"");
        modules.push(m);
    }
    let (tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut out = std::io::stdout();
    loop {
        while let Ok(line) = lines.try_recv() {
            rt.inject_frame("serial", 0, 0, line.as_bytes());
        }
        rt.run(tick);
        for (_token, frame) in rt.take_link_tx("serial") {
            let _ = out.write_all(&frame);
        }
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(tick as u64));
    }
}
