//! End-to-end host-runtime validation of Wasm `mod_mux` + Wasm `mod_rtest` (`probe` `8.2`)
//! driven by `dmesh-cli`'s `quic-mux` client over both real TCP (`listen_network`) and
//! PPP serial (`PORT_MUX = 245`).

mod common;
use common::module;
use dmesh_cli::mux_cli::{
    MuxClientTarget, PPP_MUX_MTU, ProbeDirection, ProbeServiceRequest, TokioMux, TokioMuxConfig,
    connect_mux_client, mux_probe,
};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};
use tokio::sync::mpsc;
use xip_abi::cbor;
use xip_runtime_host::{Config, NativeTx, Runtime};

#[tokio::test(flavor = "current_thread")]
async fn mod_mux_serves_probe_all_directions_over_tcp_and_serial_link() {
    let rt = Runtime::new(Config {
        pool_slots: 32,
        inbox: 16,
        mem_quota: 65_536,
        ..Default::default()
    });

    let rtest = rt.load_module(module("mod_rtest")).unwrap();
    rtest.start(b"probe");

    // Register the native serial link after `mod_rtest` so only `mod_mux` (`--port 245`)
    // binds to the host-runtime's single `"serial"` link subscription.
    rt.add_native_link("serial", xip_abi::PKT_MAX, NativeTx::Done);
    let mux_mod = rt.load_module(module("mod_mux")).unwrap();
    let ports = mux_mod
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    let tcp_addr = ports.stream.expect("mod_mux declares --stream 4");
    mux_mod.start(b"");
    rt.run(0);

    // Verify initial `mux.status` (`52.1`) before any traffic.
    let status0 = mux_mod.request(52, 1, &[0xa0]).expect("mux status");
    assert_eq!(cbor::get_int(&status0, 1).unwrap(), Some(0));

    let rt_serial = rt.clone();
    let mux_diag = mux_mod.clone();
    let rtest_diag = rtest.clone();
    let client = async move {
        // 1. Real TCP socket -> Wasm `mod_mux` -> `xip_stream` -> Wasm `mod_rtest` (`probe` `8.2`)
        let target = MuxClientTarget {
            tcp: Some(tcp_addr),
            ..Default::default()
        };
        let (cli_mux, _acc, _guard) = connect_mux_client(&target, TokioMuxConfig::default())
            .await
            .expect("connect mux TCP");

        for (idx, direction) in [
            ProbeDirection::Upload,
            ProbeDirection::Download,
            ProbeDirection::Both,
        ]
        .into_iter()
        .enumerate()
        {
            let mut req = ProbeServiceRequest::new(24_577, 1024);
            req.direction = direction;
            req.verify = true;
            let res = match mux_probe(&cli_mux, req, 100 + (idx as u64) * 10).await {
                Ok(r) => r,
                Err(e) => {
                    panic!(
                        "mux_probe over TCP ({direction:?}) failed: {e}; mux ended={} res={}, rtest ended={} res={}, status={:?}",
                        mux_diag.ended(),
                        mux_diag.result(),
                        rtest_diag.ended(),
                        rtest_diag.result(),
                        mux_diag.request(52, 1, &[0xa0]),
                    );
                }
            };
            let expected_total = match direction {
                ProbeDirection::Both => 2 * 24_577,
                _ => 24_577,
            };
            assert_eq!(res.bytes, expected_total);
            assert_eq!(res.callback_errors, [0; 6]);
        }
        cli_mux.close();
        drop(cli_mux);

        // 2. Native serial link (`PORT_MUX = 245`) -> Wasm `mod_mux` -> `xip_stream` -> Wasm `mod_rtest`
        let (ser_mux, _ser_acc) = TokioMux::spawn(true, TokioMuxConfig::default());
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        ser_mux.add_packet_link(out_tx, in_rx, PPP_MUX_MTU, 20);

        let bridge_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bridge_stop_bg = bridge_stop.clone();
        let rt_pump = rt_serial.clone();
        let serial_bridge = tokio::task::spawn_local(async move {
            while !bridge_stop_bg.load(std::sync::atomic::Ordering::Acquire) {
                let mut moved = false;
                while let Ok(frame) = out_rx.try_recv() {
                    assert_eq!(rt_pump.inject_frame("serial", 245, 0, &frame), 0);
                    rt_pump.run(0);
                    moved = true;
                }
                for (token, reply) in rt_pump.take_link_tx("serial") {
                    assert_eq!(token, 245);
                    if in_tx.send(reply).await.is_err() {
                        return;
                    }
                    moved = true;
                }
                if !moved {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });

        let mut ser_req = ProbeServiceRequest::new(12_289, 512);
        ser_req.direction = ProbeDirection::Both;
        ser_req.verify = true;
        let ser_res = mux_probe(&ser_mux, ser_req, 500)
            .await
            .expect("mux_probe over serial link 245");
        assert_eq!(ser_res.bytes, 2 * 12_289);
        assert_eq!(ser_res.callback_errors, [0; 6]);

        ser_mux.close();
        bridge_stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = serial_bridge.await;
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            tokio::select! {
                _ = rt.run_realtime(Duration::from_secs(10)) => panic!("mod_mux host-runtime test timed out"),
                _ = client => {},
            }
        })
        .await;

    rt.run(0);
    let status1 = mux_mod.request(52, 1, &[0xa0]).expect("mux status after traffic");
    let received_packets = cbor::get_int(&status1, 4).unwrap().unwrap_or(0);
    let sent_frames = cbor::get_int(&status1, 5).unwrap().unwrap_or(0);
    assert!(received_packets > 0, "expected received_packets > 0, got {status1:02x?}");
    assert!(sent_frames > 0, "expected sent_frames > 0, got {status1:02x?}");

    mux_mod.stop();
    rtest.stop();
    rt.run(0);
    assert!(mux_mod.ended());
    assert!(rtest.ended());
    assert_eq!(mux_mod.mem_leaked(), 0);
    assert_eq!(mux_mod.leaked(), 0);
    assert_eq!(rtest.mem_leaked(), 0);
    assert_eq!(rtest.leaked(), 0);
    assert_eq!(rt.packets_in_use(), 0);
}
