//! `mod_console` against the host-runtime: the lines of the serial port, the module calls, and the parse/render halves, with
//! `mod_test` as the module it talks to. (The cases of the former C simulator, tests/host/test_console.c.)
mod common;
use common::*;
use xip_abi::cbor::{self, Writer};
use xip_abi::*;
use xip_runtime_host::{Instance, Runtime};

fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 1100];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().expect("fits").len();
    buf.truncate(n);
    buf
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Console {
    rt: Runtime,
    c: Instance,
    /// instances the console talks to, run along with it
    failures: Vec<String>,
}

impl Console {
    fn new() -> Console {
        let rt = Runtime::default();
        // XIP_CONSOLE=mod_con runs the same cases against the Rust console
        let name = std::env::var("XIP_CONSOLE").unwrap_or_else(|_| "mod_console".into());
        let c = rt.add_module(module(&name), 32).unwrap();
        c.set_component(20);
        c.start(b"");
        rt.run(0);
        Console { rt, c, failures: Vec::new() }
    }

    fn fail(&mut self, msg: String) {
        eprintln!("FAIL {msg}");
        self.failures.push(msg);
    }

    fn parse(&self, line: &str) -> Vec<u8> {
        let req = enc(|w| {
            w.map(1).uint(1).text(line);
        });
        self.c.request(20, 1, &req).unwrap_or_default()
    }

    fn local_text(r: &[u8]) -> Option<String> {
        cbor::get_text(r, 12).ok().flatten().map(|t| String::from_utf8_lossy(t).into_owned())
    }

    fn check_request(&mut self, line: &str, comp: i64, method: i64, fields_hex: &str) {
        let r = self.parse(line);
        let c = cbor::get_int(&r, 1).ok().flatten().unwrap_or(-1);
        let m = cbor::get_int(&r, 2).ok().flatten().unwrap_or(-1);
        let mut h = String::new();
        if let Ok(Some(f)) = cbor::get(&r, 5) {
            let whole = f.rest();
            let mut k = cbor::Reader::new(whole);
            k.skip();
            h = hex(&whole[..whole.len() - k.rest().len()]);
        }
        if c != comp || m != method || h != fields_hex {
            let t = Self::local_text(&r).unwrap_or_default();
            self.fail(format!("line \"{line}\": got {c}.{m} fields {h} local \"{t}\", want {comp}.{method} {fields_hex}"));
        }
    }

    fn check_local(&mut self, line: &str, want: &str) {
        let r = self.parse(line);
        match Self::local_text(&r) {
            Some(t) if t.contains(want) => {}
            t => self.fail(format!("line \"{line}\": {t:?} lacks \"{want}\"")),
        }
    }

    fn check_render(&mut self, res: &[u8], comp: u64, method: u64, want: &str) {
        let req = enc(|w| {
            w.map(3).uint(1).uint(comp).uint(2).uint(method).uint(6);
        });
        let mut req = req;
        req.extend_from_slice(res);
        let ans = self.c.request(20, 2, &req).unwrap_or_default();
        let text = cbor::get_text(&ans, 1).ok().flatten().map(|t| String::from_utf8_lossy(t).into_owned()).unwrap_or_default();
        if text != want {
            self.fail(format!("render: got \"{text}\" want \"{want}\""));
        }
    }

    /// Let the console and the modules it talks to exchange packets.
    fn pump(&self) {
        for _ in 0..4 {
            self.rt.run(0);
        }
    }

    fn serial(&self) -> String {
        String::from_utf8_lossy(&self.rt.state().serial_out).into_owned()
    }

    /// A typed line, as the serial port delivers it. The answer is what the console writes to the port: exactly `want`
    /// (when it ends with a newline) or containing it.
    fn check_line(&mut self, line: &str, want: &str) {
        self.rt.state_mut().serial_out.clear();
        let rc = self.c.send(20, 3, 0, line.as_bytes());
        self.pump();
        let out = self.serial();
        let exact = want.ends_with('\n');
        if rc != 0 || (if exact { out != want } else { !out.contains(want) }) {
            self.fail(format!("line \"{line}\": got \"{out}\" (send {rc}), want \"{want}\""));
        }
    }
}

unsafe extern "C" fn silent_handle(_h: *const Host, _s: *mut u8, _c: *const Call) -> i32 {
    0
}

#[test]
fn the_console() {
    let mut k = Console::new();

    k.check_request("test.echo value=hi", 2, 1, "a101626869");
    k.check_request("test.echo value=\"two words\"", 2, 1, "a1016974776f20776f726473");
    k.check_request("test.echo @1=7", 2, 1, "a1016137");
    k.check_local("nosuch", "unknown method");
    k.check_local("status bogus=1", "unknown method");

    // answers
    let r = enc(|w| {
        w.map(1).uint(1).text("hi there");
    });
    k.check_render(&r, 2, 1, "value=\"hi there\"\n");
    let r = enc(|w| {
        w.map(0);
    });
    k.check_render(&r, 2, 3, "ok\n");
    let r = enc(|w| {
        w.map(2).uint(1).int(-68).uint(77).bytes(&[0x0a, 0x0b]);
    });
    k.check_render(&r, 99, 99, "@1=-68 @77=hex:0a0b\n");

    // the console as the serial port's owner: lines in, text out
    assert!(k.rt.state().serial_component == 20 && k.rt.state().serial_method == 3);
    {
        let mut s = k.rt.state_mut();
        s.modules_cbor = enc(|w| {
            w.array(2);
            w.map(3).uint(1).text("hello").uint(2).uint(1).uint(3).boolean(false);
            w.map(4).uint(1).text("test").uint(2).uint(2).uint(3).boolean(true).uint(5).uint(7);
        });
        s.events_cbor = enc(|w| {
            w.array(1);
            w.map(3).uint(1).text("test").uint(2).uint(32511).uint(3).text("pass=28 fail=0");
        });
    }
    k.check_line("status", "{1:{4:\"main\"},2:{},3:{},4:[{1:\"hello\",2:1,3:false},{1:\"test\",2:2,3:true,5:7}],5:{}}\n");
    k.check_line("events", "module=test id=32511 pass=28 fail=0\n");
    k.check_line("start test serve it", "ok\n");
    assert!(k.rt.state().started == "test" && k.rt.state().start_request == "serve it");
    k.check_line("start nosuch", "error: unknown module\n");
    k.check_line("start", "error: name is required\n");
    k.check_line("stop test", "ok\n");
    assert_eq!(k.rt.state().stopped, "test");
    k.check_line("reset", "ok\n");
    assert!(k.rt.state().reset_called);
    k.check_line("", "");
    k.check_line("help", "status | start NAME");
    k.check_line("nosuch", "error: unknown method");
    k.check_line("test.echo value=hi", "error: module not running (start it)\n");

    let test = k.rt.add_module(module("mod_test"), 64).unwrap();
    test.start(b"serve");
    test.set_component(2);
    k.pump();
    k.check_line("test.echo value=hello", "value=hello\n");
    k.check_line("test.echo hello there", "value=\"hello there\"\n");
    k.check_line("wifi", "state=off wanted_ssid=\"\" psk=unset\n");
    k.check_line("wifi ssid=lab psk=\"two words\"", "state=up ssid=\"lab\" ip=192.168.49.7 rssi=-40 ch=6 wanted_ssid=\"lab\" psk=set\n");
    k.check_line("wifi start", "state=up ssid=\"lab\" ip=192.168.49.7 rssi=-40 ch=6 wanted_ssid=\"lab\" psk=set\n");
    let (wanted, generation) = k.rt.wifi_intent();
    assert!(&wanted.sta_ssid[..3] == b"lab" && &wanted.sta_psk[..9] == b"two words" && wanted.radio == 1 && wanted.sta == 1 && generation == 1);
    k.check_line("wifi stop", "state=off");
    k.check_line("heap", "free=41000 largest=30000 min_free=28000 total=230000 internal_free=38000 internal_largest=26000\n");
    k.check_line("heap regions", "ok (in the device log)\n");
    k.check_line("heap bogus", "error: heap [regions");
    k.check_line("nv wifi.rx_static", "wifi.rx_static is unknown or not set\n");
    k.check_line("nv wifi.rx_static=12", "wifi.rx_static set (applies at the next start)\n");
    k.check_line("nv wifi.rx_static", "wifi.rx_static=12\n");
    k.check_line("nv bogus", "bogus is unknown or not set\n");
    k.check_line("wifi scan", "ssid=\"costin\" ch=6 rssi=-41 auth=3 bssid=10:bd:a3:ac:5a:21\nssid=\"\" ch=11 rssi=-80 auth=0 bssid=01:02:03:04:05:06\n");
    k.check_line("wifi bogus", "error: wifi [ssid=NAME]");
    k.check_line("wifi ssid=", "state=off ch=6 wanted_ssid=\"\" psk=set\n");

    // the console waits for nothing: two requests in flight, answers in any order, and messages nobody asked for
    k.rt.state_mut().serial_out.clear();
    assert_eq!(k.c.send(20, 3, 0, b"test.echo value=one"), 0);
    assert_eq!(k.c.send(20, 3, 0, b"test.echo value=two"), 0);
    k.pump();
    let out = k.serial();
    assert!(out.contains("value=one\n") && out.contains("value=two\n"), "{out:?}");
    k.rt.state_mut().serial_out.clear();
    let (a1, b1) = ([0xa1, 0x01, 0x61, b'a'], [0xa1, 0x01, 0x61, b'b']);
    assert_eq!(k.c.send_from(2, 2, 1, 0, &a1), 0, "from the module, no request: a line it chose to write");
    assert_eq!(k.c.send_from(2, 2, 1, 77, &b1), 0, "and another, with an id of its own");
    k.pump();
    assert_eq!(k.serial(), "value=a\nvalue=b\n");

    // the manifest of the installed module is its own word on its methods
    k.rt.state_mut().manifest = enc(|w| {
        w.map(3).uint(1).text("test").uint(2).uint(2).uint(3).array(2);
        w.map(2).uint(1).uint(1).uint(2).text("echo");
        w.map(2).uint(1).uint(2).uint(2).text("info");
    });
    k.check_line("test.echo value=listed", "value=listed\n");
    k.check_line("test.pkt_echo", "does not list method 20 in its manifest"); // in the catalog, not in this image
    k.rt.state_mut().manifest.clear();
    k.check_line("test.pkt_echo", ""); // no manifest: the catalog decides

    // a module that never answers costs the console nothing
    test.stop();
    k.rt.run(0);
    assert!(test.ended());
    let silent = k.rt.add_handler("silent", silent_handle, 0);
    silent.set_component(2);
    k.rt.state_mut().serial_out.clear();
    assert_eq!(k.c.send(20, 3, 0, b"test.echo value=x"), 0);
    k.rt.run(0);
    assert!(k.serial().is_empty() || !k.serial().contains("value=x"));
    k.check_line("status", "{1:{4:\"main\"}");
    k.c.stop();
    k.rt.run(0);
    assert_eq!(k.rt.state().serial_component, -1);

    assert!(k.failures.is_empty(), "{:#?}", k.failures);
}

/// `component.method` of a host component is called through its table, not sent to a module: the fields of the line fill the C
/// struct, the output struct comes back as text (native.h, generated from `x-c` of api.json).
#[test]
fn sleep_and_promiscuous_receive_are_exercised_from_the_console() {
    let mut k = Console::new();
    // promiscuous receive needs the radio on
    k.check_line("wifi.monitor enable=true channel=6", "error: wifi failed: -2\n");
    k.check_line("wifi.set radio=true sta=false", "generation=1\n");
    k.check_line("wifi.monitor enable=true channel=6", "ok\n");
    assert_eq!(k.rt.wifi_monitor().map(|m| (m.enable, m.channel)), Some((1, 6)));
    k.check_line("wifi.monitor channel=14", "error: wifi failed: -1\n");
    k.check_line("wifi.monitor enable=false", "ok\n");
    // sleep: three cycles of 500 ms, the modules get HOST_WAKE after each
    k.check_line("power.sleep duration_ms=500 cycles=3 awake_ms=100", "ok\n");
    let r = k.rt.power_requests();
    assert_eq!((r.len(), r[0].duration_ms, r[0].cycles, r[0].awake_ms), (1, 500, 3, 100));
    k.check_line("power.sleep duration_ms=5", "error: power failed: -1\n");
    k.check_line("power.info", "sleep_attempts=3 sleep_entries=3");
    k.check_line("power.dump_tasks", "ok\n");
    assert!(k.failures.is_empty(), "{:?}", k.failures);
}

#[test]
fn a_method_of_a_host_component_is_called_through_its_table() {
    let mut k = Console::new();
    k.check_line("wifi.get", "radio=false sta=false ap=false channel=0\n");
    k.check_line("wifi.set radio=true sta=true channel=6 sta_ssid=home sta_psk=password", "generation=1\n");
    let (wanted, generation) = k.rt.wifi_intent();
    assert!(wanted.radio == 1 && wanted.sta == 1 && wanted.ap == 0 && wanted.channel == 6 && generation == 1);
    assert!(&wanted.sta_ssid[..4] == b"home" && wanted.sta_ssid_len == 4 && &wanted.sta_psk[..8] == b"password" && wanted.sta_psk_len == 8);
    assert_eq!(wanted.size as usize, std::mem::size_of::<WifiIntent>());
    k.check_line("wifi.get", "radio=true sta=true ap=false channel=6 sta_ssid=home sta_psk=set\n");
    // the whole intent is replaced: what the line does not say is the default again
    k.check_line("wifi.set ap=true radio=true ap_ssid=lab bssid=hex:010203040506", "generation=2\n");
    k.check_line("wifi.get", "radio=true sta=false ap=true channel=0 bssid=hex:010203040506 ap_ssid=lab\n");
    // the host refuses what it does not accept; the console refuses what does not fit the struct
    k.check_line("wifi.set channel=15 radio=true", "error: wifi failed: -1\n");
    k.check_line("wifi.set sta_ssid=0123456789012345678901234567890123456789", "error: sta_ssid is too long\n");
    k.check_line("wifi.set bssid=hex:0102", "error: bssid has the wrong length\n");
    k.check_line("wifi.set channel=300", "error: channel is out of range\n");
    // a second component, with switches and 16-bit numbers
    k.check_line("wifi.set_tuning ampdu=2 rx_static=12", "ok\n");
    assert_eq!((k.rt.wifi_tuning().ampdu, k.rt.wifi_tuning().rx_static), (TUNE_OFF, 12));
    k.check_line("wifi.tuning", "ampdu=2 rates_11b=0 raw_rx=0 driver_tx=0 bssid_check=0 raw_tx_rate=0 rx_static=12 rx_dynamic=0 tx_dynamic=0 rx_ba=0\n");
    k.check_line("wifi.set_tuning rx_static=1", "error: wifi-tuning failed: -1\n");
    k.check_line("wifi.state", "error: not a call the console can make");
    assert!(k.failures.is_empty(), "{:?}", k.failures);
}
