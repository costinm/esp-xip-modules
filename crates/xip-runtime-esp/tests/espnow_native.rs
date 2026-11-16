//! Actual C radio, socket and shared-poller ownership/lifecycle regressions.
#![cfg(not(target_os = "espidf"))]
use std::{path::PathBuf, process::Command};

#[test]
fn native_radio_socket_and_poller_ownership() { run_native(false); }
#[test]
fn native_packet_transfer_busy_and_shared_prefix_framing() { run_native(true); }
fn run_native(packets_only:bool) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fake = root.join("crates/xip-runtime-esp/tests/native/espnow");
    let out=root.join(if packets_only {"target/espnow-packet-tests"} else {"target/espnow-native-tests"});
    std::fs::create_dir_all(&out).unwrap();
    // Compile the actual owner setup and runtime TX functions with boundary
    // fakes; extracting source avoids maintaining a second implementation.
    let wifi = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_wifi.c")).unwrap();
    let setup = wifi.split("/* BEGIN NOW RADIO SETUP:").nth(1).unwrap();
    let setup = setup
        .split_once("*/")
        .unwrap()
        .1
        .split("/* END NOW RADIO SETUP */")
        .next()
        .unwrap();
    std::fs::write(out.join("wifi_now_setup.inc"), setup).unwrap();
    let runtime =
        std::fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    fn function(source: &str, signature: &str) -> String {
        let start = source.rfind(signature).unwrap();
        let body = source[start..].find('{').unwrap() + start;
        let mut depth = 0;
        for (i, c) in source[body..].char_indices() {
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
                if depth == 0 {
                    return source[start..body + i + 1].to_string();
                }
            }
        }
        panic!("unterminated C function {signature}");
    }
    let mut tx=String::new();
    for signature in [
        "static void serial_release(",
        "static void drop_slot(",
        "static xip_packet_ref packet_take(",
        "static int link_tx(",
        "static int link_send(",
        "static int c_link_tx_done(",
        "static bool link_route(",
    ] {
        tx.push_str(&function(&runtime, signature));
        tx.push('\n');
    }
    tx.push_str(&std::fs::read_to_string(root.join("components/xip_runtime/src/port_bind.inc")).unwrap());
    tx.push('\n');
    std::fs::write(out.join("runtime_tx.inc"), tx).unwrap();
    std::fs::write(out.join("runtime_init.inc"), function(&runtime, "esp_err_t xip_host_init_simple(")).unwrap();
    std::fs::write(out.join("wifi_monitor.inc"), function(&wifi, "int xip_wifi_owner_monitor(")).unwrap();
    std::fs::write(out.join("wifi_install_simple.inc"), function(&wifi, "void xip_wifi_install_simple(")).unwrap();
    std::fs::write(out.join("wifi_boot_network.inc"), function(&wifi, "void xip_wifi_boot_network(")).unwrap();
    // BSD sockaddr has sin6_len; the host POSIX structure does not. Only
    // remove that platform field; all socket-owner/polling code is unchanged.
    let sockets = std::fs::read_to_string(root.join("components/xip_runtime/src/socket_hal.inc")).unwrap();
    std::fs::write(out.join("socket_hal_test.inc"), sockets.replace(".sin6_len=sizeof(addr),", "")).unwrap();
    let poll = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_lwip_io.c")).unwrap();
    let poll = poll.lines().filter(|line| !line.starts_with("#include")).collect::<Vec<_>>().join("\n");
    std::fs::write(out.join("lwip_io_test.inc"), poll.replace(".sin6_len=sizeof(address), ", "")).unwrap();
    let intent = std::fs::read_to_string(root.join("components/xip_runtime/src/xip_wifi_intent.c")).unwrap();
    std::fs::write(out.join("wifi_intent.inc"), intent.lines().filter(|line| !line.starts_with("#include")).collect::<Vec<_>>().join("\n")).unwrap();
    let library = out.join("driver.so");
    let status = Command::new("cc")
        .args([
            "-std=gnu11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fPIC",
            "-shared",
            "-pthread",
        ])
        .arg("-I")
        .arg(&fake)
        .arg("-I")
        .arg(root.join("components/xip_runtime/include"))
        .arg("-I")
        .arg(root.join("components/xip_runtime/src"))
        .arg("-I")
        .arg(root.join("include"))
        .arg("-I")
        .arg(&out)
        .arg(fake.join("driver.c"))
        .arg(fake.join("runtime_tx.c"))
        .arg(fake.join("socket_poll.c"))
        .arg(fake.join("owner_init.c"))
        .arg("-Wno-unused-function")
        .arg(root.join("components/xip_runtime/src/xip_pool.c"))
        .arg("-o")
        .arg(&library)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "compile the actual driver, not a Rust copy"
    );
    unsafe {
        let lib = libloading::Library::new(library).unwrap();
        lib.get::<unsafe extern "C" fn()>(b"test_radio_setup")
            .unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_runtime_ownership")
            .unwrap()();
        if packets_only {
            lib.get::<unsafe extern "C" fn(u32)>(b"test_begin").unwrap()(3);
            let offset=lib.get::<unsafe extern "C" fn()->u32>(b"test_aligned_offset").unwrap()();
            let send=lib.get::<unsafe extern "C" fn(u32,u32,i32)->i32>(b"test_send").unwrap();
            assert_eq!(send(offset,1,0),1);
            assert_eq!(send(offset,1,0x101),-5);
            for shifted in 144..152 {
                assert_eq!(send(shifted,1,0),1,"shared relay prefix at offset {shifted}");
                assert_eq!(send(shifted,1,0x101),-5,"BUSY retains payload at offset {shifted}");
            }
            lib.get::<unsafe extern "C" fn()>(b"xip_link_espnow_stop").unwrap()();
            assert_eq!(lib.get::<unsafe extern "C" fn()->u32>(b"test_contexts").unwrap()(),0);
            assert_eq!(lib.get::<unsafe extern "C" fn()->u32>(b"test_timers").unwrap()(),0);
            return;
        }
        lib.get::<unsafe extern "C" fn()>(b"test_socket_poll").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_monitor_owner").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_native_io_poll").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_real_udp_poll").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_mixed_module_and_native_poll").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_listener_retry").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_owner_init").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_c_wifi_intent").unwrap()();
        lib.get::<unsafe extern "C" fn()>(b"test_boot_network").unwrap()();
        let begin = lib.get::<unsafe extern "C" fn(u32)>(b"test_begin").unwrap();
        let stop = lib
            .get::<unsafe extern "C" fn()>(b"xip_link_espnow_stop")
            .unwrap();
        let contexts = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_contexts")
            .unwrap();
        let timers = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_timers")
            .unwrap();
        let available = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_available")
            .unwrap();
        let recv = lib
            .get::<unsafe extern "C" fn(u32, u32)>(b"test_receive")
            .unwrap();
        let count = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_received")
            .unwrap();
        let consume = lib.get::<unsafe extern "C" fn()>(b"test_consume").unwrap();
        let send = lib
            .get::<unsafe extern "C" fn(u32, u32, i32) -> i32>(b"test_send")
            .unwrap();
        let offset = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_aligned_offset")
            .unwrap();
        let reused = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_reused")
            .unwrap();
        let ready = lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_ready")
            .unwrap();
        begin(3);
        lib.get::<unsafe extern "C" fn()>(b"test_cache_exhaustion")
            .unwrap()();
        assert_eq!((contexts(), timers()), (1, 1));
        recv(0, 0); // generic callback's STA interface is zero, not an invalid pointer
        assert_eq!((count(), available()), (1, 2));
        consume();
        assert_eq!(available(), 3);
        recv(1, 0); // the link delivers repeated datagrams without protocol dedup
        assert_eq!(count(), 2);
        consume();
        assert_eq!(available(), 3);
        assert_eq!(send(offset(), 0, 0), 1);
        assert_eq!(reused(), 1, "unique aligned packet: prefix only");
        assert_eq!(send(144, 0, 0), 1);
        assert_eq!(
            reused(),
            1,
            "standard payload offset now transmits in place"
        );
        assert_eq!(
            send(offset(), 1, 0),
            1,
            "shared immutable payload permits synchronous prefix framing"
        );
        assert_eq!(send(offset(), 0, 0x101), -5);
        assert!(ready() > 0);
        stop();
        assert_eq!((contexts(), timers()), (0, 0));
        recv(0, 0); // late callback after disable must not touch freed state
        assert_eq!(count(), 2);
        begin(3); // a fresh enabled epoch
        recv(2, 1); // rejected response used to leave action_parse_busy set forever
        recv(0, 0);
        assert_eq!(count(), 1);
        consume();
        stop();
        begin(1);
        assert_eq!(
            send(145, 0, 0),
            1,
            "unaligned payload uses vendor-IE padding without a copy"
        );
        assert_eq!(available(), 1);
        stop();
        assert_eq!((contexts(), timers()), (0, 0));
        // Stop must detach the entry point before waiting, but keep the epoch
        // allocation alive until an already-entered callback returns.
        let hold = *lib
            .get::<unsafe extern "C" fn(u32)>(b"test_hold_receive")
            .unwrap();
        let entered = *lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_receive_entered")
            .unwrap();
        let enabled = *lib
            .get::<unsafe extern "C" fn() -> u32>(b"test_enabled")
            .unwrap();
        begin(3);
        hold(1);
        std::thread::scope(|scope| {
            let receiver = *recv;
            let rx = scope.spawn(move || receiver(0, 0));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while entered() == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            let stopper = *stop;
            let end = scope.spawn(move || stopper());
            while enabled() != 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert_eq!(contexts(), 1, "entered callback pins epoch state");
            hold(0);
            rx.join().unwrap();
            end.join().unwrap();
        });
        consume();
        assert_eq!((contexts(), timers()), (0, 0));
        // Fixed raw-wire vectors are also checked by the portable DMesh peer.
        let encode = lib
            .get::<unsafe extern "C" fn(
                *mut u8,
                usize,
                *const u8,
                *const u8,
                *const u8,
                *const u8,
                usize,
            ) -> usize>(b"test_encode")
            .unwrap();
        let parse = lib
            .get::<unsafe extern "C" fn(*const u8, usize, *mut u8, usize) -> std::ffi::c_long>(
                b"test_parse_body",
            )
            .unwrap();
        fn unhex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }
        fn mac(s: &str) -> [u8; 6] {
            s.split(':')
                .map(|b| u8::from_str_radix(b, 16).unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        }
        for line in include_str!("data/espnow-frames.txt")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            let fields: Vec<_> = line.split(' ').collect();
            if fields[0] == "encode" {
                let n: usize = fields[5].parse().unwrap();
                let payload: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
                let (dst, src, bssid) = (mac(fields[2]), mac(fields[3]), mac(fields[4]));
                let mut c = vec![0; xip_abi::PKT_MAX as usize + 40];
                let cn = encode(
                    c.as_mut_ptr(),
                    c.len(),
                    dst.as_ptr(),
                    src.as_ptr(),
                    bssid.as_ptr(),
                    payload.as_ptr(),
                    payload.len(),
                );
                assert_eq!(&c[..cn], unhex(fields[6]));
            } else if fields[0] == "parse-body" {
                let body = unhex(fields[1]);
                let mut c = vec![0; xip_abi::PKT_MAX as usize];
                let cn = parse(body.as_ptr(), body.len(), c.as_mut_ptr(), c.len());
                if fields[2] == "none" {
                    assert_eq!(cn, -1);
                } else {
                    let n: usize = fields[2].parse().unwrap();
                    assert_eq!(cn, n as std::ffi::c_long);
                    assert_eq!(&c[..n], unhex(fields[3]));
                }
            }
        }
        // Exercise the production boundary through the actual C codec, with
        // enough output capacity that an oversized input cannot fail merely
        // because the destination buffer is short.
        let limit = xip_abi::PKT_MAX as usize;
        let mac = [0u8; 6];
        let payload: Vec<u8> = (0..=limit).map(|i| (i * 73) as u8).collect();
        let mut frame = vec![0; limit + 41];
        let n = encode(frame.as_mut_ptr(), frame.len(), mac.as_ptr(), mac.as_ptr(),
            mac.as_ptr(), payload.as_ptr(), limit);
        assert_eq!(n, limit + 40);
        let mut decoded = vec![0; limit + 1];
        assert_eq!(parse(frame[24..].as_ptr(), n - 24, decoded.as_mut_ptr(), decoded.len()), limit as _);
        assert_eq!(&decoded[..limit], &payload[..limit]);
        assert_eq!(encode(frame.as_mut_ptr(), frame.len(), mac.as_ptr(), mac.as_ptr(),
            mac.as_ptr(), payload.as_ptr(), limit + 1), 0);
        frame[n] = payload[limit];
        assert_eq!(parse(frame[24..].as_ptr(), n + 1 - 24, decoded.as_mut_ptr(), decoded.len()), -1);
        lib.get::<unsafe extern "C" fn()>(b"test_finish").unwrap()();
    }
}
