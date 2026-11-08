//! Actual flash Wasm owns station startup and recovery; no other modules.
use std::path::{Path, PathBuf};
use xip_runtime_host::Runtime;
fn image() -> PathBuf {
    std::env::var_os("XIP_HOST_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules"))
        .join("mod_flash.wasm")
}
#[test]
fn standalone_start_activates_saved_profile_and_releases_service_state() {
    let rt = Runtime::default();
    rt.set_saved_wifi("costin").unwrap();
    let m = rt.load_module(image()).unwrap();
    m.start(&[]);
    rt.run(0);
    let (i, g) = rt.wifi_intent();
    assert!(g > 0);
    assert_eq!((i.radio, i.sta, i.ap, i.now), (1, 1, 0, 0));
    assert_eq!(&i.sta_ssid[..i.sta_ssid_len as usize], b"costin");
    m.stop();
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.leaked(), 0);
}
#[test]
fn unprovisioned_service_hosts_rescue_after_scan_without_other_modules() {
    let rt = Runtime::default();
    let m = rt.load_module(image()).unwrap();
    m.start(&[]);
    rt.run(16000);
    let (i, _) = rt.wifi_intent();
    assert_eq!((i.radio, i.sta, i.ap, i.now), (1, 0, 1, 0));
    assert_eq!(&i.ap_ssid[..i.ap_ssid_len as usize], b"ESP-RESCUE");
    assert_eq!(&i.ap_psk[..i.ap_psk_len as usize], b"untrusted-open-mode");
    m.stop();
    rt.run(0);
    assert_eq!(m.leaked(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn announcements_are_not_discovery_probes_or_echoed_back() {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };
    use tokio::net::UdpSocket;
    let rt = Runtime::default();
    rt.set_saved_wifi("costin").unwrap();
    let m = rt.load_module(image()).unwrap();
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    m.start(&[]);
    let client = async {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = [0; 1024];
        udp.send_to(
            b"esp-wifi-loader v=1 mode=main state=update",
            ports.datagram.unwrap(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), udp.recv_from(&mut buf))
                .await
                .is_err(),
            "announcements must not produce replies"
        );
        udp.send_to(b"discover", ports.datagram.unwrap())
            .await
            .unwrap();
        let (n, _) = udp.recv_from(&mut buf).await.unwrap();
        assert_eq!(xip_abi::cbor::get_int(&buf[..n],0),Ok(Some(44)));
        assert_eq!(xip_abi::cbor::get_int(&buf[..n],1),Ok(Some(1)));
        assert_eq!(xip_abi::cbor::get_int(&buf[..n],4),Ok(Some(5227)));
        m.stop();
    };
    tokio::select! {_=rt.run_realtime(Duration::from_secs(3))=>panic!("probe timed out"),_=client=>{}}
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn low_memory_tcp_admission_does_not_leak_packets_or_service_state() {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };
    use tokio::net::TcpStream;
    for quota in [256, 512, 768, 800, 816, 824, 848, 1024] {
        let rt = Runtime::new(xip_runtime_host::Config {
            mem_quota: quota,
            ..Default::default()
        });
        rt.set_saved_wifi("costin").unwrap();
        let m = rt.load_module(image()).unwrap();
        let ports = m
            .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap();
        m.start(&[]);
        rt.run(0);
        if !m.ended() {
            let client = async {
                let _socket = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
                m.stop();
            };
            tokio::select! {_=rt.run_realtime(Duration::from_secs(1))=>panic!("admission timed out"),_=client=>{}}
            rt.run(0);
        }
        assert_eq!(m.leaked(), 0, "quota={quota}");
        assert_eq!(m.mem_leaked(), 0, "quota={quota}");
    }
}

// The host delivers decoded serial frames by subscription; the C6 check below
// exercises the actual PPP framing through dmesh-cli.
fn recovery() -> (Runtime, xip_runtime_host::Instance) {
    let rt = Runtime::default();
    rt.set_saved_wifi("saved-network").unwrap();
    let m = rt.load_module(image()).unwrap();
    assert!(m.bind_port(1) >= 0);
    rt.add_native_link("serial", 512, xip_runtime_host::NativeTx::Done);
    m.start(&[]);
    rt.run(0);
    (rt, m)
}
fn wifi_control(rt: &Runtime, method: u64, id: u64, ssid: Option<&str>, psk: Option<&str>) {
    let mut b = [0; 256];
    let mut w = xip_abi::cbor::Writer::new(&mut b);
    w.map(4).uint(1).uint(44).uint(2).uint(method).uint(3).uint(id)
        .uint(5).map(usize::from(ssid.is_some()) + usize::from(psk.is_some()));
    if let Some(ssid) = ssid { w.uint(1).text(ssid); }
    if let Some(psk) = psk { w.uint(2).text(psk); }
    assert_eq!(rt.inject_frame("serial", 1, 0, w.finish().unwrap()), 0);
    rt.run(0);
}
fn replies(rt: &Runtime) -> Vec<Vec<u8>> {
    rt.take_link_tx("serial").into_iter().map(|(_, b)| b).collect()
}
#[test]
fn recovery_join_and_info_work_with_flash_only() {
    let (rt, m) = recovery();
    // A module may have changed radio roles after mod_flash started.
    let hw = unsafe { xip_abi::Hw::new(m.host()) }.unwrap();
    let mut changed = rt.wifi_intent().0;
    changed.now = 1; changed.ap = 1;
    let mut generation = 0;
    assert_eq!(unsafe { hw.wifi().unwrap().set.unwrap()(hw.ctx(), &changed, &mut generation) }, 0);
    wifi_control(&rt, 21, 7, Some("temporary"), Some("temporary-pass"));
    rt.run(5000);
    let r = replies(&rt);
    assert_eq!(r.len(), 1);
    assert_eq!(xip_abi::cbor::get_int(&r[0], 3), Ok(Some(7)));
    let result = xip_abi::cbor::get(&r[0], 6).unwrap().unwrap();
    assert!(matches!(xip_abi::cbor::get(result.rest(), 3).unwrap().unwrap().next(),
        Some(xip_abi::cbor::Item::Bytes(b)) if b == [192, 168, 49, 7]));
    assert_eq!(xip_abi::cbor::get_text(result.rest(), 2), Ok(Some(b"temporary".as_slice())));
    let (i, _) = rt.wifi_intent();
    assert_eq!(&i.sta_psk[..i.sta_psk_len as usize], b"temporary-pass");
    assert_eq!((i.now, i.ap), (1, 1));
    wifi_control(&rt, 22, 8, None, None);
    let info = replies(&rt);
    assert_eq!(info.len(), 1);
    assert!(!info[0].windows(14).any(|b| b == b"temporary-pass"));
    assert!(rt.wifi_intent().0 == i);
    m.stop(); rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
}
#[test]
fn recovery_join_waits_for_requested_network_and_dhcp_without_blocking_info() {
    let (rt, _m) = recovery();
    let mut state = xip_abi::WifiState::default();
    state.sta = 2; state.ssid_len = 5; state.ssid[..5].copy_from_slice(b"other");
    state.ip4 = [192, 168, 1, 2];
    rt.set_wifi_state(state);
    wifi_control(&rt, 21, 9, Some("new"), None);
    rt.run(10000);
    assert!(replies(&rt).is_empty());
    wifi_control(&rt, 22, 10, None, None);
    assert_eq!(replies(&rt).len(), 1);
    state.ssid_len = 3; state.ssid[..3].copy_from_slice(b"new"); state.ip4 = [0; 4];
    rt.set_wifi_state(state); rt.run(5000);
    assert!(replies(&rt).is_empty());
    state.ip4 = [192, 168, 1, 3];
    rt.set_wifi_state(state); rt.run(5000);
    let r = replies(&rt);
    assert_eq!(r.len(), 1);
    assert_eq!(xip_abi::cbor::get_int(&r[0], 3), Ok(Some(9)));
}
#[test]
fn recovery_join_retries_do_not_extend_timeout_and_invalid_credentials_do_not_apply() {
    let (rt, _m) = recovery();
    let before = rt.wifi_intent();
    wifi_control(&rt, 21, 1, Some("temp"), Some("short"));
    let r = replies(&rt);
    assert_eq!(xip_abi::cbor::get_int(&r[0], 7), Ok(Some(xip_abi::ERR_ARG as i64)));
    assert!(rt.wifi_intent() == before);
    rt.set_wifi_state(xip_abi::WifiState::default());
    wifi_control(&rt, 21, 2, Some("missing-network"), Some("valid-pass"));
    rt.run(30000);
    assert!(replies(&rt).is_empty());
    wifi_control(&rt, 21, 2, Some("missing-network"), Some("valid-pass"));
    wifi_control(&rt, 21, 3, Some("different"), None);
    let r = replies(&rt);
    assert_eq!(xip_abi::cbor::get_int(&r[0], 7), Ok(Some(xip_abi::ERR_BUSY as i64)));
    rt.run(31000);
    let r = replies(&rt);
    assert_eq!(r.len(), 1);
    assert_eq!(xip_abi::cbor::get_int(&r[0], 3), Ok(Some(2)));
    assert_eq!(xip_abi::cbor::get_int(&r[0], 7), Ok(Some(xip_abi::ERR_TIMEOUT as i64)));
}
