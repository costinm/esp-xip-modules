//! The common QUIC receive function: classification of the first byte, the relay table, the local-termination callback, CBOR datagrams
//! (version 0x444d0002) routed by component and method, other long versions, and malformed packets. The golden packets are built from the
//! layouts of quic-lite (/ws/dmesh/crates/quic-lite, read only): `encode_long_packet`, `ConnectionId::encode`, `ShortHeader::encode`.
mod common;
use common::*;
use std::cell::RefCell;
use std::rc::Rc;
use xip_abi::cbor::{self, Writer};
use xip_abi::*;
use xip_runtime_host::quic::{self, VERSION_CBOR, VERSION_DMESH};
use xip_runtime_host::{NativeTx, QuicInfo, QuicKind, QuicPacket, Runtime};

fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 2000];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().expect("fits").len();
    buf.truncate(n);
    buf
}

/// quic-lite Initial: first 0xc0, version 0x444d0001, DCID 1 byte (0x2a), SCID 2 bytes (0x1234 -> 0x92 0x34), token length 0, length 3, pn 7, 2 payload bytes.
const INITIAL: [u8; 15] = [0xc0, 0x44, 0x4d, 0x00, 0x01, 0x01, 0x2a, 0x02, 0x92, 0x34, 0x00, 0x03, 0x07, 0xaa, 0xbb];
/// quic-lite short header: flags 0x41 (fixed bit, 2-byte packet number), DCID 0x00012345 as 4 bytes (class 101: 0xa0 ...), pn 0x0009, payload.
const SHORT4: [u8; 9] = [0x41, 0xa0, 0x01, 0x23, 0x45, 0x00, 0x09, 0xde, 0xad];
const DCID4: [u8; 4] = [0xa0, 0x01, 0x23, 0x45];

struct T {
    rt: Runtime,
    declined: Rc<RefCell<Vec<Vec<u8>>>>,
}
impl T {
    /// A runtime with the console on the serial link, a native link `ble`, the classifier's local callback (releases every packet) and the
    /// old receive function, which takes what the classifier declines.
    fn new() -> T {
        let rt = Runtime::default();
        rt.add_native_link("serial", 256, NativeTx::Done);
        rt.add_native_link("ble", 1500, NativeTx::Done);
        let c = rt.add_module(module("mod_con"), 32).unwrap();
        c.set_component(20);
        assert!(c.bind_port(PORT_CONSOLE) > 0);
        c.start(b"");
        rt.run(0);
        let declined: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
        // the callback needs the runtime to read and release the packet: it keeps the packets and the test drains them (ownership passes)
        let kept: Rc<RefCell<Vec<(QuicInfo, QuicPacket)>>> = Rc::default();
        KEPT.with(|k| *k.borrow_mut() = Some(kept.clone()));
        rt.set_quic_classifier(true);
        rt.set_quic_local(Some(move |i: &QuicInfo, p: QuicPacket| kept.borrow_mut().push((i.clone(), p))));
        let d = declined.clone();
        rt.set_quic_rx(Some(move |_n: &str, _s: u64, _g: i32, data: &[u8]| {
            d.borrow_mut().push(data.to_vec());
            true
        }));
        T { rt, declined }
    }
    /// The packets the callback owns now: their bytes, and each is given back to the pool.
    fn drain(&self) -> Vec<(QuicInfo, Vec<u8>)> {
        let kept = KEPT.with(|k| k.borrow().clone().unwrap());
        let items: Vec<_> = kept.borrow_mut().drain(..).collect();
        items.into_iter().map(|(i, p)| { let b = self.rt.quic_bytes(&p); self.rt.quic_release(p); (i, b) }).collect()
    }
    fn rx(&self, name: &str, frame: &[u8]) { assert_eq!(self.rt.inject_frame(name, 5, -40, frame), 0); self.rt.run(0); }
    fn pump(&self) { for _ in 0..4 { self.rt.run(0); } }
}
thread_local! { static KEPT: RefCell<Option<Rc<RefCell<Vec<(QuicInfo, QuicPacket)>>>>> = const { RefCell::new(None) }; }

/// A 0x444d0002 packet: long header with the given CIDs, then the envelope.
fn datagram(dcid: &[u8], scid: &[u8], envelope: &[u8]) -> Vec<u8> {
    let mut v = quic::long_build(VERSION_CBOR, dcid, scid).unwrap();
    v.extend_from_slice(envelope);
    v
}

#[test]
fn golden_headers_classify_like_quic_lite() {
    let (c, h) = quic::classify(&INITIAL, 0);
    assert_eq!((c, h.is_long, h.version, h.dcid.as_slice(), h.scid.as_slice(), h.body), (quic::QuicClass::Long, true, VERSION_DMESH, &[0x2a][..], &[0x92, 0x34][..], 10));
    let (c, h) = quic::classify(&SHORT4, 0);
    assert_eq!((c, h.is_long, h.dcid.as_slice(), h.body), (quic::QuicClass::Short, false, &DCID4[..], 5));
    assert_eq!(quic::classify(&[0x00, 1, 2, 3], 0).0, quic::QuicClass::NotQuic);
    assert_eq!(quic::classify(&[0x80, 0, 0, 0, 1, 0, 0], 0).0, quic::QuicClass::NotQuic, "the fixed bit is checked first, for both forms");
    assert_eq!(quic::classify(&[0x40, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8], 0).0, quic::QuicClass::Malformed, "reserved CID class");
    assert_eq!(quic::classify(&SHORT4[..], 3).1.dcid.len(), 3, "a fixed length overrides the class rule");
    for n in 0..INITIAL.len() { let _ = quic::classify(&INITIAL[..n], 0); }
    for n in 0..SHORT4.len() { let _ = quic::classify(&SHORT4[..n], 0); }
    assert_eq!(quic::envelope(&enc(|w| { w.map(2).uint(1).uint(20).uint(2).uint(5); })), Some((20, 5, None, false)));
    assert_eq!(quic::envelope(&enc(|w| { w.map(3).uint(1).uint(20).uint(2).uint(5).uint(3).uint(987); })), Some((20, 5, Some(987), false)));
    assert_eq!(quic::envelope(&enc(|w| { w.map(1).uint(1).uint(20); })), None);
}

#[test]
fn a_short_packet_for_a_local_dcid_goes_to_the_callback_which_owns_it() {
    let t = T::new();
    assert_eq!(t.rt.quic_table_add(&DCID4, QuicKind::Local, 0, 0, &[]), 0);
    t.rx("ble", &SHORT4);
    assert_eq!(t.rt.packets_in_use(), 1, "the callback owns the pool packet");
    let got = t.drain();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].0.link.as_str(), got[0].0.source, got[0].0.signal, got[0].0.header.dcid.as_slice()), ("ble", 5, -40, &DCID4[..]));
    assert_eq!(got[0].1, SHORT4);
    assert_eq!(t.rt.packets_in_use(), 0, "released");
    assert_eq!(t.rt.quic_stats().short_local, 1);
    assert!(t.declined.borrow().is_empty(), "nobody else sees a taken packet");
    // a 1448-byte packet is the largest; 1449 is dropped and counted
    let mut big = vec![0x41u8, 0xa0, 0x01, 0x23, 0x45, 0, 9];
    big.resize(quic::MAX_PACKET, 0x55);
    t.rx("ble", &big);
    assert_eq!(t.drain()[0].1.len(), quic::MAX_PACKET);
    big.push(0);
    t.rx("ble", &big);
    assert!(t.drain().is_empty());
    assert_eq!((t.rt.quic_stats().oversize, t.rt.packets_in_use()), (1, 0));
}

#[test]
fn an_unknown_dcid_is_dropped_and_counted_and_relay_entries_are_placeholders() {
    let t = T::new();
    t.rx("ble", &SHORT4);
    assert!(t.drain().is_empty());
    let s = t.rt.quic_stats();
    assert_eq!((s.rx, s.short_unknown, s.short_local), (1, 1, 0));
    assert_eq!(t.rt.packets_in_use(), 0);
    // a relay entry forwards: to its link and peer, with the DCID replaced (4 bytes -> 1 byte), flags, packet number and body unchanged
    assert_eq!(t.rt.quic_table_add(&DCID4, QuicKind::Relay, t.rt.link_addr("ble"), 9, &[0x2a]), 0);
    t.rx("ble", &SHORT4);
    t.pump();
    assert_eq!((t.rt.quic_stats().short_relay, t.rt.packets_in_use()), (1, 0));
    let sent = t.rt.take_link_tx("ble");
    assert_eq!(sent, vec![(9, vec![0x41, 0x2a, 0x00, 0x09, 0xde, 0xad])]);
    // and back to a 4-byte DCID (the next hop's), growing into the headroom
    assert_eq!(t.rt.quic_table_add(&DCID4, QuicKind::Relay, t.rt.link_addr("ble"), 10, &[0xc0, 1, 2, 3, 4, 5, 6, 7]), 0);
    t.rx("ble", &SHORT4);
    t.pump();
    assert_eq!(t.rt.take_link_tx("ble"), vec![(10, vec![0x41, 0xc0, 1, 2, 3, 4, 5, 6, 7, 0x00, 0x09, 0xde, 0xad])]);
    // an egress that is gone: dropped and counted, nothing queued
    assert_eq!(t.rt.quic_table_add(&DCID4, QuicKind::Relay, 0x4000_00ff, 9, &[0x2a]), 0);
    t.rx("ble", &SHORT4);
    t.pump();
    assert_eq!((t.rt.quic_stats().short_relay, t.rt.quic_stats().short_relay_failed, t.rt.packets_in_use()), (2, 1, 0));
    assert!(t.rt.take_link_tx("ble").is_empty());
    // a local entry without a callback
    assert_eq!(t.rt.quic_table_add(&DCID4, QuicKind::Local, 0, 0, &[]), 0);
    t.rt.set_quic_local(None::<fn(&QuicInfo, QuicPacket)>);
    t.rx("ble", &SHORT4);
    assert_eq!((t.rt.quic_stats().short_no_handler, t.rt.packets_in_use()), (1, 0));
    // the table: replace, remove, grow
    assert_eq!(t.rt.quic_table_remove(&DCID4), 0);
    assert_eq!(t.rt.quic_table_remove(&DCID4), ERR_NOTFOUND);
    for i in 0..quic::TABLE_MAX {
        assert_eq!(t.rt.quic_table_add(&[0xa0, 0, (i >> 8) as u8, i as u8], QuicKind::Local, 0, 0, &[]), 0);
    }
    assert_eq!(t.rt.quic_table_add(&[1], QuicKind::Local, 0, 0, &[]), ERR_NOMEM);
    assert_eq!(t.rt.quic_table_len(), quic::TABLE_MAX);
}

#[test]
fn a_cbor_datagram_is_routed_to_the_console_and_answered_as_a_cbor_datagram() {
    let t = T::new();
    let (d, s) = ([0xa0u8, 0, 0, 7], [0x2au8]);
    // console.status (20/5), with an id
    let env = enc(|w| { w.map(4).uint(1).uint(20).uint(2).uint(5).uint(3).uint(0x1_0000_0077).uint(5).map(0); });
    t.rx("ble", &datagram(&d, &s, &env));
    t.pump();
    assert_eq!(t.rt.packets_in_use(), 0);
    assert_eq!(t.rt.quic_stats().long_cbor, 1);
    let sent = t.rt.take_link_tx("ble");
    assert_eq!(sent.len(), 1, "one answer");
    assert_eq!(sent[0].0, 5, "to the token it came from");
    let (class, h) = quic::classify(&sent[0].1, 0);
    assert_eq!((class, h.version, h.dcid.as_slice(), h.scid.as_slice()), (quic::QuicClass::Long, VERSION_CBOR, &s[..], &d[..]), "the CIDs are swapped");
    let answer = &sent[0].1[h.body..];
    assert_eq!(cbor::get_int(answer, 1).unwrap(), Some(20));
    assert_eq!(cbor::get_int(answer, 2).unwrap(), Some(5));
    assert_eq!(cbor::get_int(answer, 3).unwrap(), Some(0x1_0000_0077));
    assert!(cbor::get(answer, 6).unwrap().is_some() && cbor::get(answer, 7).unwrap().is_none());
    // an unsupported method answers with the numeric error (field 7)
    let env = enc(|w| { w.map(4).uint(1).uint(20).uint(2).uint(900).uint(3).uint(78).uint(5).map(0); });
    t.rx("ble", &datagram(&d, &s, &env));
    t.pump();
    let sent = t.rt.take_link_tx("ble");
    assert_eq!(sent.len(), 1);
    let h = quic::classify(&sent[0].1, 0).1;
    let answer = &sent[0].1[h.body..];
    assert_eq!((cbor::get_int(answer, 3).unwrap(), cbor::get_int(answer, 7).unwrap()), (Some(78), Some(ERR_UNSUPPORTED as i64)));
    // a component nobody serves goes to the console too, which answers the error
    let env = enc(|w| { w.map(4).uint(1).uint(9999).uint(2).uint(1).uint(3).uint(79).uint(5).map(0); });
    t.rx("ble", &datagram(&d, &s, &env));
    t.pump();
    let sent = t.rt.take_link_tx("ble");
    let h = quic::classify(&sent[0].1, 0).1;
    assert_eq!(cbor::get_int(&sent[0].1[h.body..], 7).unwrap(), Some(ERR_UNSUPPORTED as i64));
    assert_eq!((t.rt.quic_stats().long_cbor, t.rt.packets_in_use()), (3, 0));
    // the answer prefix is not kept: a later unrelated message to the same token is not wrapped (the note was used)
    assert!(t.declined.borrow().is_empty() && t.drain().is_empty());
}

#[test]
fn bad_envelopes_and_other_long_versions() {
    let t = T::new();
    let (d, s) = ([0xa0u8, 0, 0, 7], [0x2au8]);
    for bad in [&[][..], &[0xff, 0x00], &[0xa1, 0x01, 0x14], &enc(|w| { w.map(2).uint(1).uint(0).uint(2).uint(5); })] {
        t.rx("ble", &datagram(&d, &s, bad));
    }
    t.pump();
    assert_eq!((t.rt.quic_stats().long_cbor_bad, t.rt.quic_stats().long_cbor, t.rt.packets_in_use()), (4, 0, 0));
    assert!(t.rt.take_link_tx("ble").is_empty());
    // quic-lite's version 1 packet goes to the local callback
    t.rx("ble", &INITIAL);
    let got = t.drain();
    assert_eq!((got.len(), got[0].0.header.version, got[0].1.as_slice()), (1, VERSION_DMESH, &INITIAL[..]));
    // a version that is not ours at all, too
    let mut other = INITIAL;
    other[1..5].copy_from_slice(&1u32.to_be_bytes());
    t.rx("ble", &other);
    assert_eq!(t.drain().len(), 1);
    assert_eq!(t.rt.quic_stats().long_other, 2);
    // without a callback: dropped and counted
    t.rt.set_quic_local(None::<fn(&QuicInfo, QuicPacket)>);
    t.rx("ble", &INITIAL);
    assert_eq!((t.rt.quic_stats().long_other_dropped, t.rt.packets_in_use()), (1, 0));
}

#[test]
fn malformed_and_truncated_packets_are_dropped_without_a_leak() {
    let t = T::new();
    let env = enc(|w| { w.map(3).uint(1).uint(20).uint(2).uint(5).uint(3).uint(1); });
    let cases = [datagram(&[0xa0, 0, 0, 7], &[0x2a], &env), INITIAL.to_vec(), SHORT4.to_vec()];
    for pkt in &cases {
        for n in 1..pkt.len() {
            t.rx("ble", &pkt[..n]);
            t.pump();
            let _ = t.drain();
            let _ = t.rt.take_link_tx("ble");
            assert_eq!(t.rt.packets_in_use(), 0, "truncated at {n} of {}", pkt.len());
        }
    }
    let s = t.rt.quic_stats();
    assert!(s.malformed > 0 && s.rx > 0);
    // a lying length byte (DCID beyond the packet), and a CID above 20 bytes
    t.rx("ble", &[0xc0, 0x44, 0x4d, 0x00, 0x02, 4, 1, 2]);
    t.rx("ble", &[0xc0, 0x44, 0x4d, 0x00, 0x02, 21, 0, 0]);
    assert_eq!(t.rt.quic_stats().malformed, s.malformed + 2);
    assert_eq!(t.rt.packets_in_use(), 0);
}

#[test]
fn what_is_not_quic_is_declined_to_the_old_path_and_other_links_are_untouched() {
    let t = T::new();
    t.rx("ble", &[0x00, 1, 2, 3]);       // fixed bit clear
    assert_eq!(t.declined.borrow().len(), 1);
    assert_eq!((t.rt.quic_stats().declined, t.rt.quic_stats().rx), (1, 1));
    // a link that is not a QUIC link never reaches the classifier: the old function sees even a short-header-looking frame
    t.rt.add_native_link("espnow", 1500, NativeTx::Done);
    t.rt.add_native_link("mylink", 1500, NativeTx::Done);
    t.rx("mylink", &SHORT4);
    assert_eq!((t.declined.borrow().len(), t.rt.quic_stats().rx), (2, 1));
    t.rt.quic_link_enable("mylink");
    t.rx("mylink", &SHORT4);
    assert_eq!((t.declined.borrow().len(), t.rt.quic_stats().short_unknown), (2, 1));
    // ESP-NOW is a QUIC link by default (the NAN carrier goes to the NAN port before the link)
    t.rx("espnow", &SHORT4);
    assert_eq!(t.rt.quic_stats().short_unknown, 2);
    assert_eq!(t.rt.packets_in_use(), 0);
}

#[test]
fn the_classifier_is_off_until_enabled_so_an_old_receive_function_keeps_its_frames() {
    let rt = Runtime::default();
    rt.add_native_link("ble", 1500, NativeTx::Done);
    let seen = Rc::new(RefCell::new(0));
    let s = seen.clone();
    rt.set_quic_rx(Some(move |_: &str, _: u64, _: i32, _: &[u8]| { *s.borrow_mut() += 1; true }));
    assert_eq!(rt.inject_frame("ble", 1, 0, &SHORT4), 0);
    rt.run(0);
    assert_eq!((*seen.borrow(), rt.quic_stats().rx, rt.packets_in_use()), (1, 0, 0));
}

#[test]
fn an_answer_is_never_dispatched_as_a_request() {
    let t = T::new();
    let (d, s) = ([0xa0u8, 0, 0, 7], [0x2au8]);
    // what the console answered earlier, arriving on the other device: component, method and id of a request plus a result (6) or an error (7)
    let answer = enc(|w| { w.map(4).uint(1).uint(20).uint(2).uint(5).uint(3).uint(77).uint(6).map(0); });
    let failure = enc(|w| { w.map(4).uint(1).uint(20).uint(2).uint(5).uint(3).uint(77).uint(7).int(-2); });
    t.rx("ble", &datagram(&d, &s, &answer));
    t.rx("ble", &datagram(&d, &s, &failure));
    t.pump();
    assert!(t.rt.take_link_tx("ble").is_empty(), "no ping-pong between two devices");
    let got = t.drain();
    assert_eq!(got.len(), 2, "the callback sees the answers");
    assert_eq!((t.rt.quic_stats().long_cbor_response, t.rt.quic_stats().long_cbor, t.rt.packets_in_use()), (2, 0, 0));
    t.rt.set_quic_local(None::<fn(&QuicInfo, QuicPacket)>);
    t.rx("ble", &datagram(&d, &s, &answer));
    assert_eq!((t.rt.quic_stats().long_cbor_response, t.rt.packets_in_use()), (3, 0));
}

#[test]
fn the_runtime_issues_cids_and_reuses_them() {
    let t = T::new();
    for i in 1..=16u8 {
        assert_eq!(t.rt.quic_table_alloc(QuicKind::Local, 0, 0, &[]).unwrap(), vec![i], "1-byte CIDs for the first entries");
    }
    assert_eq!(t.rt.quic_table_remove(&[5]), 0);
    assert_eq!(t.rt.quic_table_alloc(QuicKind::Local, 0, 0, &[]).unwrap(), vec![5], "the freed ID is reused");
    while t.rt.quic_table_len() < quic::TABLE_MAX {
        t.rt.quic_table_alloc(QuicKind::Local, 0, 0, &[]).unwrap();
    }
    assert_eq!(t.rt.quic_table_alloc(QuicKind::Local, 0, 0, &[]), Err(ERR_NOMEM));
    // quic-lite encodes 1 byte up to 0x7f, 2 bytes up to 0x1fff (13 bits: 8191 values, not 4k), 4 bytes up to 0x1fffffff
    assert_eq!(quic::cid_encode(0x7f), Some(vec![0x7f]));
    assert_eq!(quic::cid_encode(0x80), Some(vec![0x80, 0x80]));
    assert_eq!(quic::cid_encode(0x1fff), Some(vec![0x9f, 0xff]));
    assert_eq!(quic::cid_encode(0x12345), Some(DCID4.to_vec()));
    assert_eq!(quic::cid_encode(0), None);
}

#[test]
fn an_answer_is_sent_only_with_the_request_id_and_to_the_requesting_peer() {
    let t = T::new();
    let (d, s) = ([0xa0u8, 0, 0, 7], [0x2au8]);
    // two requests from two peers at once, both with id 77: each answer goes back to its own peer
    for peer in [5u64, 6] {
        let env = enc(|w| { w.map(4).uint(1).uint(20).uint(2).uint(5).uint(3).uint(77).uint(5).map(0); });
        assert_eq!(t.rt.inject_frame("ble", peer, 0, &datagram(&d, &s, &env)), 0);
    }
    t.pump();
    let mut sent = t.rt.take_link_tx("ble");
    sent.sort_by_key(|f| f.0);
    assert_eq!(sent.iter().map(|f| f.0).collect::<Vec<_>>(), vec![5, 6], "to the address the request came from");
    for f in &sent {
        assert_eq!(quic::classify(&f.1, 0).1.version, VERSION_CBOR);
    }
}
