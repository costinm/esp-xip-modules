mod common;
use common::*;
use xip_abi::*;
use xip_runtime_host::Runtime;

#[test]
fn async_echo_owns_data_across_calls_and_instances_idle_without_polling() {
    let rt = Runtime::default();
    let a = rt.add_module(module("mod_rtest"), 64).unwrap();
    let b = rt.add_module(module("mod_rtest"), 64).unwrap();
    a.set_component(2);
    b.set_component(3);
    a.start(b"async");
    b.start(b"async");
    rt.run(0);
    let calls = a.calls();
    rt.run(3600000);
    assert_eq!(a.calls(), calls);
    let available = rt.pool_available();
    let mut payload = vec![0xa1, 1, 0x63, b'o', b'n', b'e'];
    assert_eq!(a.send(2, 1, 123, &payload), 0);
    assert_eq!(b.send(2, 1, 456, &payload), 0);
    rt.run(0);
    payload.fill(0);
    assert_eq!(
        rt.pool_available(),
        available - 2,
        "each instance retains its original pool packet across the sleep"
    );
    assert!(rt.state().main.is_empty());
    rt.run(6);
    let state = rt.state();
    let replies: Vec<_> = state.main.iter().filter(|m| m.id != 0).collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].data, [0xa1, 1, 0x63, b'o', b'n', b'e']);
    assert_eq!((replies[0].id, replies[1].id), (123, 456));
    assert_eq!((replies[0].from, replies[1].from), (2, 3));
    drop(state);
    a.stop();
    b.stop();
    rt.run(0);
    assert_eq!(
        (a.mem_leaked(), b.mem_leaked(), a.leaked(), b.leaked()),
        (0, 0, 0, 0)
    );
    assert_eq!(a.active_alarms(), 0);
}

#[test]
fn stop_drops_pending_sleep_and_queued_owned_calls() {
    let (rt, m) = started("mod_rtest", "async");
    m.set_component(2);
    rt.run(0);
    for id in 1..=3 {
        assert_eq!(m.send(2, 1, id, &[0xa1, 1, 0x61, b'x']), 0);
    }
    rt.run(0);
    assert!(m.active_alarms() > 0);
    m.stop();
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.active_alarms(), 0);
    assert_eq!(m.leaked(), 0);
    rt.run(100);
    assert!(rt.state().main.iter().all(|m| m.id == 0));
}

#[test]
fn async_alarm_finishes_and_releases_future() {
    let (rt, m) = started("mod_rtest", "async-alarm");
    rt.run(29);
    assert!(!m.ended());
    rt.run(1);
    assert_eq!(m.result(), RET_DONE as i32);
    assert!(m.find_event(0x7eff, "async alarm ok").is_some());
    assert_eq!(m.mem_leaked(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn tokio_real_clock_drives_module_sleep() {
    let (rt, m) = started("mod_rtest", "async-alarm");
    rt.run_realtime(std::time::Duration::from_millis(40)).await;
    assert!(m.ended());
    assert_eq!(m.result(), RET_DONE as i32);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn self_wake_schedules_one_poll_and_can_be_cancelled() {
    let (rt, m) = started("mod_rtest", "async-yield");
    rt.run(0);
    assert!(!m.ended());
    assert_eq!(m.active_alarms(), 1);
    rt.run(1);
    assert!(
        !m.ended(),
        "second self-wake reuses the currently firing alarm slot"
    );
    assert_eq!(m.active_alarms(), 1);
    rt.run(1);
    assert!(m.ended());
    assert_eq!(m.mem_leaked(), 0);
    let (rt, m) = started("mod_rtest", "async-yield");
    rt.run(0);
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.active_alarms(), 0);
}

#[test]
fn async_inbox_overflow_replies_busy_and_keeps_running() {
    let (rt, m) = started("mod_rtest", "async");
    m.set_component(2);
    rt.run(0);
    for _ in 0..5 {
        assert_eq!(m.post(2, 1, &[0xa1, 1, 0x61, b'x']), 0);
    }
    rt.run(0);
    assert_eq!(m.send(2, 1, 123, &[0xa1, 1, 0x61, b'y']), 0);
    rt.run(0);
    assert!(!m.ended());
    assert!(
        rt.state()
            .main
            .iter()
            .any(|m| m.id == 123 && m.data == [0x24]),
        "overflow replies CBOR ERR_BUSY using original correlation"
    );
    rt.run(25);
    assert_eq!(m.events().iter().filter(|e| e.id == 0x7e10).count(), 5);
    assert!(!m.ended());
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.active_alarms(), 0);
}

#[test]
fn held_packet_quota_refusal_uses_borrowed_packet_to_reply_busy() {
    let (rt, m) = started("mod_rtest", "async");
    m.set_component(2);
    rt.run(0);
    let available = rt.pool_available();
    for id in 1..=5 {
        assert_eq!(m.send(2, 1, id, &[0xa1, 1, 0x61, b'x']), 0);
        rt.run(0);
    }
    assert!(!m.ended());
    assert_eq!(m.held_count(), 4);
    assert_eq!(rt.pool_available(), available - 4);
    assert!(
        rt.state()
            .main
            .iter()
            .any(|c| c.id == 5 && c.data == [0x24]),
        "quota refusal replies from incoming packet without allocating another held lease"
    );
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
    assert_eq!(rt.pool_available(), available);
}

#[test]
fn async_startup_under_insufficient_heap_returns_nomem_without_leaks() {
    let rt = Runtime::new(xip_runtime_host::Config {
        mem_quota: 1,
        ..Default::default()
    });
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    m.start(b"async");
    rt.run(0);
    assert!(m.ended());
    assert_eq!(m.result(), ERR_NOMEM);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn executor_allocation_failures_return_nomem_and_release_partial_startup() {
    // Exercise queue, shared context, signal, future, Running, and sleep setup
    // allocation failures under the real module allocator, rather than a mock.
    for quota in [256, 384, 512, 640, 768, 896, 1024, 1152, 1280, 1536, 2048] {
        let rt = Runtime::new(xip_runtime_host::Config {
            mem_quota: quota,
            ..Default::default()
        });
        let m = rt.add_module(module("mod_rtest"), 64).unwrap();
        m.start(b"async-alarm");
        rt.run(30);
        assert!(m.ended(), "quota {quota}");
        assert!(
            m.result() == ERR_NOMEM || m.result() == RET_DONE as i32,
            "quota {quota}: result {}",
            m.result()
        );
        assert_eq!(m.mem_leaked(), 0, "quota {quota}");
        assert_eq!(m.active_alarms(), 0, "quota {quota}");
    }
}

#[test]
fn packetless_snapshot_allocation_failure_drops_call_then_recovers() {
    let rt = Runtime::new(xip_runtime_host::Config {
        mem_quota: 1400,
        ..Default::default()
    });
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    m.set_component(2);
    m.start(b"async");
    rt.run(0);
    assert!(
        !m.ended(),
        "executor starts within the configured heap quota"
    );
    assert_eq!(m.post(2, 1, &[0; 1024]), 0);
    rt.run(0);
    assert!(
        !m.ended(),
        "failed snapshot allocation does not abort or cancel the task"
    );
    assert!(m.events().is_empty());
    assert_eq!(m.post(2, 1, &[0xa1, 1, 0x61, b'z']), 0);
    rt.run(5);
    assert!(!m.ended());
    assert!(m.find_event(0x7e10, "z").is_some());
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
}

#[test]
fn successive_sleeps_complete_queued_echoes() {
    let (rt, m) = started("mod_rtest", "async");
    m.set_component(2);
    rt.run(0);
    for id in 1..=3 {
        assert_eq!(m.send(2, 1, id, &[0xa1, 1, 0x61, b'x']), 0);
    }
    rt.run(16);
    let ids: Vec<_> = rt
        .state()
        .main
        .iter()
        .filter(|m| m.id != 0)
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, [1, 2, 3]);
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn async_datagram_echo_preserves_link_token_and_payload() {
    let rt = Runtime::default();
    rt.add_native_link("serial", 256, xip_runtime_host::NativeTx::Done);
    let m = rt.add_module(module("mod_rtest"), 64).unwrap();
    m.set_component(2);
    assert!(m.bind_port(PORT_CONSOLE) >= 0);
    m.start(b"async");
    rt.run(0);
    let payload = [0, 0x7e, 0x7d, 0xff];
    assert_eq!(rt.inject_frame("serial", 3, 0, &payload), 0);
    rt.run(0);
    assert!(rt.take_link_tx("serial").is_empty());
    rt.run(5);
    assert_eq!(rt.take_link_tx("serial"), vec![(3, payload.to_vec())]);
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
    assert_eq!(m.leaked(), 0);
}
