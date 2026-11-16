//! Explicit weighted routes carry complete endpoint packets across multiple hops.
use std::{cell::RefCell, rc::Rc};
use xip_abi::*;
use xip_runtime_host::{NativeTx, QuicInfo, QuicPacket, Runtime};
unsafe extern "C" fn idle(_: *const Host, _: *mut u8, _: *const Call) -> i32 {
    RET_CONTINUE as i32
}
fn route(cid: u8, link: u32, token: u64, rewrite: u8, endpoint: bool) -> RelayRoute {
    let mut r = RelayRoute::default();
    r.dcid[0] = cid;
    r.dcid_len = 1;
    r.hop_count = 1;
    r.endpoint = endpoint as u8;
    r.hops[0] = RelayHop {
        link,
        token,
        weight: 1,
        cid_len: 1,
        ..Default::default()
    };
    r.hops[0].rewritten_dcid[0] = rewrite;
    r
}
fn table(h: &Host) -> &Quic {
    unsafe { &*h.get_component.unwrap()(h.user, c"quic".as_ptr()).cast::<Quic>() }
}
#[test]
fn explicit_multihop_endpoint_preserves_long_packets_and_the_reverse_circuit() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1500, NativeTx::Done);
    rt.add_native_link("ble", 1500, NativeTx::Done);
    let owner = rt.add_handler("disc", idle, 0);
    owner.set_component(47);
    owner.start(&[]);
    rt.run(0);
    let h = owner.host();
    let t = table(h);
    let udp = rt.link_addr("udp");
    let ble = rt.link_addr("ble");
    let forward = route(2, ble, 10, 3, false);
    let terminal = route(3, udp, 20, 1, true);
    unsafe {
        assert_eq!(t.relay_set.unwrap()(h.user, &forward, 0), 0);
        assert_eq!(t.relay_set.unwrap()(h.user, &terminal, 0), 0);
    }
    let kept: Rc<RefCell<Vec<(QuicInfo, QuicPacket)>>> = Rc::default();
    let capture = kept.clone();
    rt.set_quic_local(Some(move |i: &QuicInfo, p: QuicPacket| {
        capture.borrow_mut().push((i.clone(), p))
    }));
    // A complete long-header handshake is opaque to intermediate forwarding.
    let inner = [0xc0, 0x44, 0x4d, 0, 1, 1, 42, 1, 24, 0, 3, 7, 0xaa, 0xbb];
    let mut outer = vec![0x40, 2, 0];
    outer.extend(inner);
    assert_eq!(rt.inject_frame("udp", 9, -40, &outer), 0);
    let forwarded = rt.take_link_tx("ble");
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].0, 10);
    assert_eq!(&forwarded[0].1[..3], &[0x40, 3, 0]);
    assert_eq!(&forwarded[0].1[3..], &inner);
    assert_eq!(rt.inject_frame("ble", 10, -41, &forwarded[0].1), 0);
    let (info, packet) = kept.borrow_mut().pop().unwrap();
    assert_eq!(info.link, "relay");
    assert_eq!(info.source, 3);
    assert_eq!(info.signal, -41);
    assert_eq!(rt.quic_bytes(&packet), inner);
    rt.quic_send(packet, "relay", info.source).unwrap();
    let response = rt.take_link_tx("udp");
    assert_eq!(response.len(), 1);
    assert_eq!(response[0].0, 20);
    assert_eq!(&response[0].1[..3], &[0x40, 1, 0]);
    assert_eq!(&response[0].1[3..], &inner);
    assert_eq!(rt.packets_in_use(), 0);
    owner.stop();
    rt.run(0);
    assert_eq!(rt.quic_table_len(), 0);
}
#[test]
fn route_replacement_is_atomic_weighted_and_owner_scoped() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1500, NativeTx::Done);
    rt.add_native_link("ble", 1500, NativeTx::Done);
    let owner = rt.add_handler("disc", idle, 0);
    owner.set_component(47);
    owner.start(&[]);
    let other = rt.add_handler("other", idle, 0);
    other.set_component(49);
    other.start(&[]);
    rt.run(0);
    let h = owner.host();
    let t = table(h);
    let mut r = route(2, rt.link_addr("ble"), 11, 3, false);
    r.hop_count = 2;
    r.hops[1] = r.hops[0];
    r.hops[1].token = 12;
    r.hops[1].weight = 2;
    unsafe {
        assert_eq!(t.relay_set.unwrap()(h.user, &r, 1), ERR_NOTFOUND);
        assert_eq!(t.relay_set.unwrap()(h.user, &r, 0), 0);
        assert_eq!(t.relay_set.unwrap()(other.host().user, &r, 1), ERR_BUSY);
    }
    for _ in 0..30 {
        assert_eq!(rt.inject_frame("udp", 0, 0, &[0x40, 2, 0, 9]), 0);
    }
    let frames = rt.take_link_tx("ble");
    assert_eq!(frames.iter().filter(|f| f.0 == 11).count(), 10);
    assert_eq!(frames.iter().filter(|f| f.0 == 12).count(), 20);
    r.hops[0].weight = 0;
    r.hops[1].weight = 0;
    unsafe {
        assert_eq!(t.relay_set.unwrap()(h.user, &r, 1), ERR_ARG);
    }
    assert_eq!(rt.inject_frame("udp", 0, 0, &[0x40, 2, 0, 9]), 0);
    assert_eq!(rt.take_link_tx("ble")[0].0, 11);
    other.stop();
    rt.run(0);
    assert_eq!(rt.quic_table_len(), 1);
    owner.stop();
    rt.run(0);
    assert_eq!(rt.quic_table_len(), 0);
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn allocated_relays_reuse_live_rules_track_data_use_and_expire_without_recycling_ids() {
    let rt = Runtime::default();
    rt.add_native_link("udp", 1500, NativeTx::Done);
    rt.add_native_link("ble", 1500, NativeTx::Done);
    let owner = rt.add_handler("disc", idle, 0);
    owner.set_component(47);
    owner.start(&[]);
    rt.run(0);
    let h = owner.host();
    let t = table(h);
    let mut request = route(1, rt.link_addr("ble"), 11, 3, false);
    request.dcid = [0; 20];
    request.dcid_len = 0;
    let mut first = request;
    unsafe {
        assert_eq!(t.relay_create.unwrap()(h.user, &mut first, 100, 1), 0);
    }
    assert_eq!(first.dcid_len, 8);
    let mut reused = request;
    unsafe {
        assert_eq!(t.relay_create.unwrap()(h.user, &mut reused, 100, 1), 1);
    }
    assert_eq!(reused.dcid, first.dcid);
    rt.run(70);
    let mut packet = vec![0x40];
    packet.extend_from_slice(&first.dcid[..8]);
    packet.extend([0, 9]);
    assert_eq!(rt.inject_frame("udp", 0, 0, &packet), 0);
    assert_eq!(rt.take_link_tx("ble").len(), 1);
    let mut stats = RelayStats::default();
    unsafe {
        assert_eq!(
            t.relay_stats.unwrap()(h.user, first.dcid.as_ptr(), 8, &mut stats),
            0
        );
    }
    assert_eq!(stats.last_used_us, 70000);
    rt.run(70);
    unsafe {
        assert_eq!(t.relay_expire.unwrap()(h.user), 0);
    }
    // Reads/configuration do not make an inactive circuit immortal.
    unsafe {
        assert_eq!(
            t.relay_stats.unwrap()(h.user, first.dcid.as_ptr(), 8, &mut stats),
            0
        );
    }
    assert_eq!(stats.last_used_us, 70000);
    rt.run(31);
    assert_eq!(rt.inject_frame("udp", 0, 0, &packet), 0);
    assert!(rt.take_link_tx("ble").is_empty());
    unsafe {
        assert_eq!(t.relay_expire.unwrap()(h.user), 1);
    }
    let mut next = request;
    unsafe {
        assert_eq!(t.relay_create.unwrap()(h.user, &mut next, 0, 0), 0);
    }
    assert_ne!(next.dcid, first.dcid);
    rt.run(10000);
    unsafe {
        assert_eq!(t.relay_expire.unwrap()(h.user), 0);
    }
    owner.stop();
    rt.run(0);
    assert_eq!(rt.quic_table_len(), 0);
    assert_eq!(rt.packets_in_use(), 0);
}

#[test]
fn host_config_scales_relay_routes_beyond_small_c_runtime_limit() {
    // 1. Default runtime enforces small 64-entry C-runtime limit.
    let small_rt = Runtime::default();
    assert_eq!(small_rt.quic_table_max(), 64);
    let ble_small = small_rt.add_native_link("ble", 1448, NativeTx::Done);
    for i in 0..64u64 {
        assert!(
            small_rt
                .quic_table_alloc(xip_runtime_host::quic::QuicKind::Relay, ble_small, i + 1, &[0x33])
                .is_ok()
        );
    }
    assert_eq!(
        small_rt.quic_table_alloc(xip_runtime_host::quic::QuicKind::Relay, ble_small, 99, &[0x33]),
        Err(ERR_NOMEM)
    );

    // 2. Host runtime (`Runtime::host()`) scales to thousands of concurrent routes with zero leaks.
    let host_rt = Runtime::host();
    assert_eq!(host_rt.quic_table_max(), xip_runtime_host::quic::HOST_TABLE_MAX);
    host_rt.add_native_link("udp", 1500, NativeTx::Done);
    host_rt.add_native_link("ble", 1500, NativeTx::Done);
    let ble = host_rt.link_addr("ble");
    let owner = host_rt.add_handler("disc", idle, 0);
    owner.set_component(47);
    owner.start(&[]);
    host_rt.run(0);
    let h = owner.host();
    let t = table(h);

    let mut created_cids = Vec::with_capacity(512);
    for i in 0..512u64 {
        let mut request = RelayRoute {
            hop_count: 1,
            ..Default::default()
        };
        request.hops[0] = RelayHop {
            link: ble,
            token: 10_000 + i,
            weight: 1,
            cid_len: 2,
            ..Default::default()
        };
        request.hops[0].rewritten_dcid[0] = 0x44;
        request.hops[0].rewritten_dcid[1] = (i & 0xff) as u8;
        unsafe {
            assert_eq!(t.relay_create.unwrap()(h.user, &mut request, 30_000, 0), 0);
        }
        assert_eq!(request.dcid_len, 8);
        created_cids.push(request.dcid);
    }
    assert_eq!(host_rt.quic_table_len(), 512);

    // Forward packets across multiple routes across the 512-route table and verify zero packet leaks.
    for idx in [0usize, 63, 64, 255, 511] {
        let mut packet = vec![0x40];
        packet.extend_from_slice(&created_cids[idx][..8]);
        packet.extend([0x01, 0xaa, 0xbb]);
        assert_eq!(host_rt.inject_frame("udp", 0, 0, &packet), 0);
        let out = host_rt.take_link_tx("ble");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 10_000 + idx as u64);
        assert_eq!(out[0].1, vec![0x40, 0x44, (idx & 0xff) as u8, 0x01, 0xaa, 0xbb]);
    }

    owner.stop();
    host_rt.run(0);
    assert_eq!(host_rt.quic_table_len(), 0);
    assert_eq!(host_rt.packets_in_use(), 0);
}
