//! Readiness of native tokio sockets (the host counterpart of the C socket owner's
//! readiness and polling checks): data that arrived before the OPEN notification is
//! still reported, and an idle runtime waits instead of polling.
mod common;
use common::module;
use std::{cell::RefCell, net::{IpAddr, Ipv4Addr}, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream};
use xip_abi::*;
use xip_runtime_host::Runtime;

#[derive(Default)]
struct Probe {
    events: RefCell<Vec<u32>>,
    data: RefCell<Vec<u8>>,
    seen: tokio::sync::Notify,
}
unsafe extern "C" fn probe(host: *const Host, _: *mut u8, call: *const Call) -> i32 {
    let hw = Hw::new(host).unwrap();
    let p = &*hw.get_component(c"readiness-probe").cast::<Probe>();
    let c = &*call;
    if c.component == 0 {
        return RET_CONTINUE as i32;
    }
    let id = c.id as u32;
    p.events.borrow_mut().push(c.value as u32);
    if c.value as u32 == SOCKET_EVENT_READABLE {
        let mut b = [0u8; 64];
        let n = hw.socket_read(id, &mut b);
        if n > 0 {
            p.data.borrow_mut().extend_from_slice(&b[..n as usize]);
            p.seen.notify_one();
        }
    }
    RET_CONTINUE as i32
}

async fn setup(rt: &Runtime, p: &Probe) -> (xip_runtime_host::Instance, std::net::SocketAddr) {
    rt.register_component("readiness-probe", (p as *const Probe).cast());
    let loaded = xip_runtime_host::Loaded::open(module("mod_rtest")).unwrap().with_handler(probe);
    let m = rt.add_loaded("readiness-probe", loaded, 64);
    m.set_component(2);
    let ports = m.listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).await.unwrap();
    m.start(b"");
    (m, ports.stream.unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn data_sent_before_open_is_delivered_after_open() {
    let rt = Runtime::default();
    let p = Box::new(Probe::default());
    let (m, addr) = setup(&rt, &p).await;
    // The payload is in the kernel before the runtime has polled the accepted socket.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(b"early").await.unwrap();
    client.flush().await.unwrap();
    tokio::select! {
        _ = rt.run_realtime(Duration::from_secs(5)) => panic!("timed out"),
        _ = p.seen.notified() => {}
    }
    let ev = p.events.borrow().clone();
    assert_eq!(ev[0], SOCKET_EVENT_OPEN);
    assert!(ev.contains(&SOCKET_EVENT_READABLE), "{ev:?}");
    assert_eq!(&*p.data.borrow(), b"early");
    m.stop();
    rt.run(0);
    assert_eq!(m.leaked(), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_runtime_waits_until_a_peer_connects() {
    let rt = Runtime::default();
    let p = Box::new(Probe::default());
    let (m, addr) = setup(&rt, &p).await;
    // Nothing is connected: the dispatcher must stay parked and deliver nothing.
    rt.run_realtime(Duration::from_millis(300)).await;
    assert!(p.events.borrow().is_empty());
    let _client = TcpStream::connect(addr).await.unwrap();
    rt.run_realtime(Duration::from_millis(300)).await;
    assert_eq!(p.events.borrow().first(), Some(&SOCKET_EVENT_OPEN));
    m.stop();
    rt.run(0);
}
