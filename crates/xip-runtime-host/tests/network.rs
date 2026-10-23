mod common;
use common::module;
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};
use xip_runtime_host::Runtime;

#[tokio::test(flavor = "current_thread")]
async fn declared_udp_and_async_stream_echo_use_real_sockets() {
    let rt = Runtime::default();
    let m = rt.load_module(module("mod_rtest")).unwrap();
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    m.start(b"async");
    let client = async {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let data = [0, 0xff, 0x7e, 0x7d];
        udp.send_to(&data, ports.datagram.unwrap()).await.unwrap();
        let mut out = [0; 32];
        let (n, peer) = udp.recv_from(&mut out).await.unwrap();
        assert_eq!(&out[..n], data);
        assert_eq!(peer, ports.datagram.unwrap());
        let mut stream = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
        let payload: Vec<_> = (0..128 * 1024).map(|n| n as u8).collect();
        let (mut read, mut write) = stream.split();
        let mut got = vec![0; payload.len()];
        let (sent, received) = tokio::join!(write.write_all(&payload), read.read_exact(&mut got));
        sent.unwrap();
        received.unwrap();
        assert_eq!(got, payload);
        drop(stream);
        m.stop();
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(3)) => panic!("network test timed out"),
        _ = client => {},
    }
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
    assert!(std::net::TcpListener::bind(ports.stream.unwrap()).is_ok());
    assert!(std::net::UdpSocket::bind(ports.datagram.unwrap()).is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn console_tcp_admission_and_stop_close_connections_without_commands() {
    let rt = Runtime::default();
    let m = rt.load_module(module("mod_con")).unwrap();
    let ports = m.listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).await.unwrap();
    m.start(b"");
    let client = async {
        let mut first = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
        let mut second = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
        let mut byte = [0];
        assert_eq!(second.read(&mut byte).await.unwrap(), 0);
        // Admission and cleanup are valuable regardless of the console's command parser.
        assert!(tokio::time::timeout(Duration::from_millis(30), first.read(&mut byte)).await.is_err());
        m.stop();
        assert_eq!(first.read(&mut byte).await.unwrap(), 0);
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(3)) => panic!("connection admission test timed out"),
        _ = client => {},
    }
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.mem_leaked(), 0);
    assert!(std::net::TcpListener::bind(ports.stream.unwrap()).is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_stop_and_reload_reuses_udp_links() {
    let rt = Runtime::default();
    for _ in 0..20 {
        let m = rt.load_module(module("mod_rtest")).unwrap();
        let ports = m
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap();
        m.start(b"async");
        rt.run(0);
        m.stop();
        rt.run(0);
        assert_eq!(m.mem_leaked(), 0);
        assert!(std::net::TcpListener::bind(ports.stream.unwrap()).is_ok());
        assert!(std::net::UdpSocket::bind(ports.datagram.unwrap()).is_ok());
    }
}

#[test]
fn copying_message_releases_internal_packet_when_link_is_busy() {
    use xip_abi::*;
    unsafe extern "C" fn handle(host: *const Host, _: *mut u8, call: *const Call) -> i32 {
        if (*call).component == 0 && (*call).method == HOST_START as i32 {
            let hw = Hw::new(host).unwrap();
            assert_eq!(hw.message(ADDR_LINK, 1, 2, 3, b"retryable"), ERR_BUSY);
        }
        RET_CONTINUE as i32
    }
    let rt = Runtime::default();
    rt.add_native_link("busy", 256, xip_runtime_host::NativeTx::Busy);
    let m = rt.add_handler("message", handle, 1);
    let available = rt.pool_available();
    m.start(b"");
    rt.run(0);
    assert_eq!(m.held_count(), 0);
    assert_eq!(rt.pool_available(), available);
    m.stop();
    rt.run(0);
    assert_eq!(m.leaked(), 0);
}

#[derive(Default)]
struct SocketProbe {
    sent: std::cell::Cell<usize>,
    blocked: tokio::sync::Notify,
    done: tokio::sync::Notify,
    eof: tokio::sync::Notify,
    peer: std::cell::Cell<u16>,
}
unsafe extern "C" fn socket_probe(
    host: *const xip_abi::Host,
    _: *mut u8,
    call: *const xip_abi::Call,
) -> i32 {
    use xip_abi::*;
    let hw = Hw::new(host).unwrap();
    let probe = &*hw.get_component(c"socket-probe").cast::<SocketProbe>();
    let c = &*call;
    if c.component == 0 {
        return RET_CONTINUE as i32;
    }
    let id = c.id as u32;
    match c.value as u32 {
        SOCKET_EVENT_OPEN => {
            let (peer, port, scope) = hw.socket_peer(id).unwrap();
            assert_eq!(
                peer,
                std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped().octets()
            );
            assert_eq!(scope, 0);
            probe.peer.set(port);
            let bytes = [0x5au8; 4096];
            let mut sent = 0;
            loop {
                let n = hw.socket_write(id, &bytes);
                if n == ERR_AGAIN {
                    break;
                }
                assert!(n > 0);
                sent += n as usize;
                assert!(
                    sent < 32 * 1024 * 1024,
                    "kernel must eventually apply backpressure"
                );
            }
            probe.sent.set(sent);
            let pkt = hw.pkt_alloc().unwrap();
            let (span, _) = hw.pkt_span(pkt).unwrap();
            std::ptr::copy_nonoverlapping(
                b"owned packet".as_ptr(),
                span.add(PKT_HEADROOM as usize),
                12,
            );
            assert_eq!(
                hw.socket_write_pkt(u32::MAX, pkt, PKT_HEADROOM, 12),
                ERR_ARG
            );
            assert!(hw.pkt_span(pkt).is_ok(), "error retains exact lease");
            assert_eq!(hw.socket_write_pkt(id, pkt, PKT_HEADROOM, 12), 0);
            assert!(hw.pkt_span(pkt).is_err(), "accepted packet is transferred");
            let other = hw.pkt_alloc().unwrap();
            assert_eq!(hw.socket_write_pkt(id, other, PKT_HEADROOM, 1), ERR_BUSY);
            assert!(hw.pkt_span(other).is_ok());
            hw.pkt_free(other);
            assert_eq!(hw.socket_write(id, b"x"), ERR_AGAIN);
            probe.blocked.notify_one();
        }
        SOCKET_EVENT_WRITE_DONE => probe.done.notify_one(),
        SOCKET_EVENT_READABLE => {
            let mut b = [0; 8];
            let n = hw.socket_read(id, &mut b);
            assert!(n == 0 || n == ERR_AGAIN);
        }
        SOCKET_EVENT_EOF => {
            probe.eof.notify_one();
            hw.socket_close(id);
        }
        _ => {}
    }
    RET_CONTINUE as i32
}

#[tokio::test(flavor = "current_thread")]
async fn socket_backpressure_packet_ownership_peer_and_eof() {
    let rt = Runtime::default();
    let fixture = Box::new(SocketProbe::default());
    rt.register_component("socket-probe", (&*fixture as *const SocketProbe).cast());
    // Use generated manifest for declarative ports, then the probe handler to
    // check the actual socket ABI with native Rust allocations.
    let mut loaded = xip_runtime_host::Loaded::open(module("mod_rtest")).unwrap();
    loaded.handler = socket_probe;
    let m = rt.add_loaded("socket-probe", loaded, 64);
    m.set_component(2);
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    m.start(b"");
    let available = rt.pool_available();
    let client = async {
        let mut stream = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
        fixture.blocked.notified().await;
        assert_eq!(fixture.peer.get(), stream.local_addr().unwrap().port());
        let mut data = vec![0; fixture.sent.get() + 12];
        stream.read_exact(&mut data).await.unwrap();
        assert!(data[..fixture.sent.get()].iter().all(|b| *b == 0x5a));
        assert_eq!(&data[fixture.sent.get()..], b"owned packet");
        fixture.done.notified().await;
        stream.shutdown().await.unwrap();
        fixture.eof.notified().await;
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(5)) => panic!("backpressure test timed out"),
        _ = client => {},
    }
    m.stop();
    rt.run(0);
    assert_eq!(m.held_count(), 0);
    assert_eq!(m.leaked(), 0);
    assert_eq!(rt.pool_available(), available);
}

#[tokio::test(flavor = "current_thread")]
async fn local_producer_wakes_idle_dispatcher_without_timer_polling() {
    unsafe extern "C" fn handle(
        h: *const xip_abi::Host,
        _: *mut u8,
        c: *const xip_abi::Call,
    ) -> i32 {
        if (*c).component == 7 {
            let hw = xip_abi::Hw::new(h).unwrap();
            (&*hw.get_component(c"wake-test").cast::<tokio::sync::Notify>()).notify_one();
        }
        xip_abi::RET_CONTINUE as i32
    }
    let rt = Runtime::default();
    let arrived = Box::new(tokio::sync::Notify::new());
    rt.register_component(
        "wake-test",
        (&*arrived as *const tokio::sync::Notify).cast(),
    );
    let m = rt.add_handler("wake", handle, 1);
    m.start(b"");
    rt.run(0);
    let producer = async {
        tokio::task::yield_now().await;
        assert_eq!(m.post(7, 1, b"local task"), 0);
        arrived.notified().await;
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(2)) => panic!("post did not wake idle runtime"),
        _ = producer => {},
    }
    m.stop();
    rt.run(0);
}

#[tokio::test(flavor = "current_thread")]
async fn stop_cancels_async_socket_wait_and_closes_connection() {
    let rt = Runtime::default();
    let m = rt.load_module(module("mod_rtest")).unwrap();
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    m.start(b"async");
    let client = async {
        let mut stream = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
        stream.write_all(b"before stop").await.unwrap();
        let mut data = [0; 11];
        stream.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"before stop");
        // The module returned to next_call with this accepted connection open.
        m.stop();
        assert_eq!(stream.read(&mut data).await.unwrap(), 0);
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(3)) => panic!("stop did not close accepted stream"),
        _ = client => {},
    }
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
    assert!(std::net::TcpListener::bind(ports.stream.unwrap()).is_ok());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_realtime_steps_keep_fractional_elapsed_time() {
    use core::future::Future;
    let rt = Runtime::default();
    for _ in 0..20 {
        let mut step = Box::pin(rt.step_realtime(Duration::from_secs(100)));
        core::future::poll_fn(|cx| {
            assert!(step.as_mut().poll(cx).is_pending());
            core::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_micros(500)).await;
        drop(step);
    }
    rt.run_realtime(Duration::ZERO).await;
    assert_eq!(
        rt.now(),
        10,
        "cancellation cannot discard the fractional elapsed epoch"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn udp_peer_churn_beyond_capacity_keeps_reply_tokens_correct() {
    let rt = Runtime::default();
    let m = rt.load_module(module("mod_rtest")).unwrap();
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    m.start(b"async");
    let client = async {
        let mut peers = Vec::new();
        for id in 0u32..66 {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            socket
                .send_to(&id.to_le_bytes(), ports.datagram.unwrap())
                .await
                .unwrap();
            let mut reply = [0; 4];
            socket.recv_from(&mut reply).await.unwrap();
            assert_eq!(reply, id.to_le_bytes());
            peers.push(socket);
        }
        peers[0]
            .send_to(b"back", ports.datagram.unwrap())
            .await
            .unwrap();
        let mut reply = [0; 4];
        peers[0].recv_from(&mut reply).await.unwrap();
        assert_eq!(&reply, b"back");
    };
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(3)) => panic!("UDP peer churn timed out"),
        _ = client => {},
    }
    m.stop();
    rt.run(0);
    assert_eq!(m.leaked(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cli_defaults_bind_declared_ports_and_serve_async_echo() {
    use tokio::io::AsyncBufReadExt;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_xip-host"))
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        for _ in 0..2 {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("CLI exited during startup");
            assert!(line.contains("TCP Some"), "{line}");
        }
        let mut echo = TcpStream::connect("127.0.0.1:5228").await.unwrap();
        echo.write_all(b"CLI async").await.unwrap();
        let mut reply = [0; 9];
        echo.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"CLI async");
    })
    .await;
    child.kill().await.unwrap();
    let _ = child.wait().await;
    result.unwrap();
}

#[test]
fn binary_console_reports_registered_modules_and_controls_real_instance() {
    use xip_abi::{cbor::{self, Item, Reader, Writer}, ERR_UNSUPPORTED};
    let rt = Runtime::default();
    let console = rt.load_module(module("mod_con")).unwrap();
    let test = rt.load_module(module("mod_rtest")).unwrap();
    console.start(b"");
    test.start(b"async");
    rt.run(0);
    let status = console.request(20, 5, &[0xa0]).unwrap();
    let mut slots = cbor::get(&status, 4).unwrap().expect("module list");
    let Some(Item::Array(count)) = slots.next() else { panic!("module list must be an array") };
    let mut names = Vec::new();
    for _ in 0..count {
        let slot = slots.rest();
        names.push(cbor::get_text(slot, 1).unwrap().unwrap().to_vec());
        if names.last().unwrap() == b"test" {
            assert_eq!(cbor::get(slot, 4).unwrap().unwrap().next(), Some(Item::Bool(true)));
        }
        assert!(slots.skip());
    }
    assert!(names.iter().any(|n| n == b"console"));
    assert!(names.iter().any(|n| n == b"test"));

    // Assert the exact structured result rather than a rendered line.
    let echo = b"\xa1\x01\x68registry";
    assert_eq!(test.send(2, 1, 1234, echo), 0);
    rt.run(6);
    {
        let state = rt.state();
        let reply = state.main.iter().find(|msg| msg.component == 2 && msg.method == 1 && msg.id == 1234)
            .expect("correlated async echo");
        assert_eq!(cbor::get_text(&reply.data, 1).unwrap(), Some(b"registry".as_slice()));
    }

    let name = b"\xa1\x01\x64test";
    assert_eq!(console.request(20, 7, name).unwrap(), [0xa0]);
    rt.run(0);
    assert!(test.ended());
    assert_eq!(test.mem_leaked(), 0);
    // Host native instances cannot currently restart in place; retain this regression.
    let mut buf = [0; 64];
    let mut w = Writer::new(&mut buf);
    w.map(2).uint(1).text("test").uint(2).bytes(b"async");
    let restart = console.request(20, 6, w.finish().unwrap()).unwrap();
    assert_eq!(Reader::new(&restart).next().and_then(|item| item.as_int()), Some(ERR_UNSUPPORTED as i64));
    console.stop();
    rt.run(0);
    assert_eq!(console.mem_leaked(), 0);
}

#[test]
fn paused_device_bench_rejects_overflow_then_answers_admitted_requests() {
    let rt = Runtime::default();
    let m = rt.load_module(module("mod_rtest")).unwrap();
    m.start(b"async-pause");
    rt.run(0);
    let payload = [0xa1, 1, 0x61, b'x'];
    for id in 1..=6 {
        assert_eq!(m.send(2, 1, id, &payload), 0);
        rt.run(0);
    }
    assert!(!m.ended());
    assert_eq!(
        rt.state()
            .main
            .iter()
            .filter(|m| m.id != 0 && m.data == [0x24])
            .count(),
        2
    );
    rt.run(1100);
    assert_eq!(
        rt.state()
            .main
            .iter()
            .filter(|m| m.id != 0 && m.data == payload)
            .count(),
        4
    );
    assert_eq!(m.send(2, 1, 7, &payload), 0);
    rt.run(6);
    assert!(rt
        .state()
        .main
        .iter()
        .any(|m| m.id == 7 && m.data == payload));
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
}
