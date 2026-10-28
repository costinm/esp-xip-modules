//! Discovery display and the shared discovery.nodes signed-record stream.
use super::*;
use std::collections::BTreeMap;

pub(super) fn vip(a: &announce::Announce) -> String {
    announce::virtual_ip6(a.public_key())
        .map(|ip| hex_encode(&ip[8..]))
        .unwrap_or_else(|| {
            let id = a.device_id();
            if id.len() >= 8 {
                hex_encode(&id[..8])
            } else {
                "?".into()
            }
        })
}

pub(super) fn mac(a: &announce::Announce, endpoint: SocketAddr) -> Option<[u8; 6]> {
    let source = match endpoint.ip() {
        std::net::IpAddr::V6(ip) => Some(ip.octets()),
        _ => None,
    };
    [source, a.sta_link_local_v6(), a.udp_link_local_v6()]
        .into_iter().flatten().find_map(|ip| {
            (ip[..8] == [0xfe,0x80,0,0,0,0,0,0] && ip[11..13] == [0xff,0xfe])
                .then_some([ip[8]^2,ip[9],ip[10],ip[13],ip[14],ip[15]])
        })
}

fn prefix(reporter: &str, vip: &str, mac: Option<[u8; 6]>) -> String {
    format!(
        "reporter={reporter} vip={vip} mac={}",
        mac.map(|m| mac_encode(&m)).unwrap_or_else(|| "?".into())
    )
}

pub(super) fn profile_prefix(profile: &DeviceProfile) -> String {
    let vip = profile
        .vip6
        .map(|ip| hex_encode(&ip.octets()[8..]))
        .unwrap_or_else(|| "?".into());
    prefix("local", &vip, profile.mac)
}

pub(super) fn association_error(identity: &str, endpoint: SocketAddr, error: String) -> String {
    if error.to_ascii_lowercase().contains("association") {
        format!("{identity} status=association_failed endpoint={endpoint} error={error:?}")
    } else {
        error
    }
}

pub(super) fn multicast_line(a: &announce::Announce, endpoint: SocketAddr) -> String {
    let id = vip(a);
    format!(
        "{} via=multicast endpoint={endpoint} authenticated={} {}",
        prefix(&id, &id, mac(a, endpoint)),
        a.has_identity() && announce::verify_identity(*a),
        announce_log_fields(a).join(" ")
    )
}

pub(super) fn run() -> Result<(), String> {
    let peers = multicast_discover_peers()?;
    if peers.is_empty() {
        return Err("no peer answered UDP6 multicast discovery".into());
    }
    // One stream at a time shares the host packet pool without creating a
    // private packet inventory per observer. Each stream owns its own window.
    for peer in &peers {
        let id = fresh_request_id();
        let request =
            encode_stream_command_with_id("discovery.nodes", id).map_err(|e| e.to_string())?;
        match exchange_discovery_nodes(peer.peer, &request) {
            Ok(bytes) => match decode_nodes(&vip(&peer.announce), &bytes) {
                Ok(rows) => {
                    let mut latest = BTreeMap::new();
                    for row in rows {
                        latest.insert(
                            (row.display_key.clone(), row.peer_mac, row.bearer.clone()),
                            row.fields,
                        );
                    }
                    for line in latest.into_values() {
                        println!("{line}");
                    }
                }
                Err(error) => println!(
                    "{} status=discovery_failed endpoint={} error={error:?}",
                    prefix(
                        &vip(&peer.announce),
                        &vip(&peer.announce),
                        mac(&peer.announce, peer.peer)
                    ),
                    peer.peer
                ),
            },
            Err(error) => println!(
                "{} status=association_failed endpoint={} error={error:?}",
                prefix(
                    &vip(&peer.announce),
                    &vip(&peer.announce),
                    mac(&peer.announce, peer.peer)
                ),
                peer.peer
            ),
        }
    }
    Ok(())
}

fn take_item<'a>(data: &'a [u8], d: &mut minicbor::Decoder<'a>) -> Result<&'a [u8], String> {
    let at = d.position();
    d.skip().map_err(|e| e.to_string())?;
    Ok(&data[at..d.position()])
}

pub(super) fn decode_nodes(reporter: &str, data: &[u8]) -> Result<Vec<ObservedNode>, String> {
    use dmesh_server::discovery_nodes as wire;
    let mut d = minicbor::Decoder::new(data);
    let header = take_item(data, &mut d)?;
    let header = dmesh_server::tagged::decode(header).ok_or("missing discovery response header")?;
    if !wire::is_header(header) {
        return Err("unsupported discovery.nodes format".into());
    }
    let mut rows = Vec::new();
    while d.position() < data.len() {
        let item = take_item(data, &mut d)?;
        if item == wire::END {
            return if d.position() == data.len() {
                Ok(rows)
            } else {
                Err("data after discovery completion".into())
            };
        }
        let row = wire::Observation::decode(item).ok_or("invalid discovery observation")?;
        let Some(a) = announce::decode_announce(row.record) else {
            continue;
        };
        if a.device_id().is_empty() {
            continue;
        }
        let verified = a.has_identity() && announce::verify_identity(a);
        let identity = if verified || !a.has_identity() {
            vip(&a)
        } else {
            "?".into()
        };
        let mut fields = vec![
            prefix(reporter, &identity, row.peer),
            format!(
                "via={} authenticated={verified} retained={} origin={}",
                row.bearer,
                row.retained,
                if row.origin == wire::Origin::Direct {
                    "direct"
                } else {
                    "crawled"
                }
            ),
        ];
        if let Some(rssi) = row.rssi_dbm {
            fields.push(format!("rssi_dbm={rssi}"));
        }
        if let Some(channel) = row.channel {
            fields.push(format!("channel={channel}"));
        }
        if let Some(age) = row.age_ms {
            fields.push(format!("age_ms={age}"));
        }
        if let Some(time) = row.last_seen_ms {
            fields.push(format!("last_seen_ms={time}"));
        }
        if let Some(via) = row.via {
            fields.push(format!("observed_by={}", hex_encode(via)));
        }
        if verified || !a.has_identity() {
            fields.extend(announce_log_fields(&a));
        } else {
            fields.push("signature=invalid".into());
        }
        rows.push(ObservedNode {
            node: if verified {
                hex_encode(a.device_id())
            } else {
                String::new()
            },
            display_key: hex_encode(a.device_id()),
            peer_mac: row.peer,
            bearer: row.bearer.to_owned(),
            fields: fields.join(" "),
        });
    }
    Err("discovery stream missing terminal record".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmesh_server::discovery_nodes::{self as wire, Observation};
    fn response(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0; 64];
        let n = wire::header(42, &mut out).unwrap();
        out.truncate(n);
        let mut row = Observation::new(payload, "now");
        row.peer = Some([2; 6]);
        row.rssi_dbm = Some(-43);
        let mut body = [0; 1024];
        let n = row.encode(&mut body).unwrap();
        out.extend(&body[..n]);
        out.extend(wire::END);
        out
    }
    #[test]
    fn display_uses_compact_identifiers_and_original_signed_fields() {
        let (payload, id) = super::super::live_observation_tests::signed_announcement();
        let rows = decode_nodes("1122334455667788", &response(&payload)).unwrap();
        assert_eq!(rows.len(), 1);
        let line = &rows[0].fields;
        assert!(
            line.starts_with(&format!(
                "reporter=1122334455667788 vip={} mac=02:02:02:02:02:02",
                hex_encode(&id[..8])
            )),
            "{line}"
        );
        assert!(line.contains("authenticated=true"));
        assert!(line.contains("uptime_secs=0"));
        assert!(!line.contains("fc00:"));
        let a = announce::decode_announce(&payload).unwrap();
        let direct = multicast_line(&a, "[fe80::12bd:a3ff:feac:5a20]:3339".parse().unwrap());
        assert!(direct.contains("mac=10:bd:a3:ac:5a:20 via=multicast"));
    }
    #[test]
    fn truncation_tampering_and_unknown_bearer_metadata_are_not_trusted() {
        let (mut payload, _) = super::super::live_observation_tests::signed_announcement();
        let bytes = response(&payload);
        for n in 0..bytes.len() {
            assert!(decode_nodes("observer", &bytes[..n]).is_err());
        }
        let a = announce::decode_announce(&payload).unwrap();
        let at = payload
            .windows(a.signature().len())
            .position(|b| b == a.signature())
            .unwrap();
        payload[at] ^= 1;
        let rows = decode_nodes("observer", &response(&payload)).unwrap();
        assert!(rows[0].node.is_empty());
        assert!(rows[0].fields.contains("signature=invalid"));
    }
    #[test]
    fn association_failure_keeps_target_identifiers() {
        let target = prefix("local", "1122334455667788", Some([2; 6]));
        let line = association_error(
            &target,
            "127.0.0.1:3339".parse().unwrap(),
            "establish association: closed".into(),
        );
        assert!(line.starts_with(
            "reporter=local vip=1122334455667788 mac=02:02:02:02:02:02 status=association_failed"
        ));
    }
    #[test]
    fn linux_android_producer_and_cli_share_fragmented_signed_stream_contract() {
        use dmesh_discovery::{Engine,UnsignedIdentity,P256Identity,NoFacts,Carrier};
        use dmesh_server::{discovery::DiscoveryPacketKind,discovery_nodes::Options};
        use std::sync::Arc;
        let sender=Engine::new(Arc::new(P256Identity::generate()),Arc::new(NoFacts));
        let original=sender.announce_wire().unwrap();
        let observer=Arc::new(Engine::new(Arc::new(UnsignedIdentity::from_seed(b"observer")),Arc::new(NoFacts)));
        observer.on_datagram(&Carrier::radio("nan","02:02:02:02:02:02",DiscoveryPacketKind::ActivePublish),&original);
        let mut reader=dmesh_discovery::nodes::stream_response(observer,42,Options{duration_ms:0,retained:true,active:false},false).unwrap().into_reader();
        let mut response=Vec::new();let mut chunk=[0;7];
        loop {let(n,done)=reader.read(&mut chunk);response.extend_from_slice(&chunk[..n]);if done{break;}assert!(n>0);}
        let rows=decode_nodes("1122334455667788",&response).unwrap();
        assert_eq!(rows.len(),1);assert!(rows[0].fields.contains("authenticated=true retained=true origin=direct"));
        assert!(rows[0].fields.contains("last_seen_ms="));
        assert!(rows[0].fields.starts_with("reporter=1122334455667788 vip="));
    }

    #[test]
    fn unsigned_android_peers_without_macs_are_not_collapsed_together(){
        let mut bytes=vec![0;64];let n=wire::header(42,&mut bytes).unwrap();bytes.truncate(n);
        for id in [1,2] {
            let a=announce::Announce::discovery([id;16],8,0);let mut record=[0;512];let n=announce::encode(a,&mut record).unwrap();
            let row=Observation::new(&record[..n],"nan");let mut out=[0;1024];let n=row.encode(&mut out).unwrap();bytes.extend_from_slice(&out[..n]);
        }
        bytes.extend(wire::END);let rows=decode_nodes("observer",&bytes).unwrap();
        let mut latest=BTreeMap::new();for row in rows {assert!(row.node.is_empty());latest.insert((row.display_key,row.peer_mac,row.bearer),row.fields);}
        assert_eq!(latest.len(),2);
    }

    #[test]
    fn multicast_mac_uses_the_actual_interface_before_cached_other_interface(){
        let mut a=announce::Announce::discovery([1;16],8,0);
        a.set_udp_link_local_v6("fe80::2c41:eeff:fe44:bd03".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(mac(&a,"[fe80::7619:f8ff:fe17:de65%7]:3339".parse().unwrap()),Some([0x74,0x19,0xf8,0x17,0xde,0x65]));
        assert_eq!(mac(&a,"[fe80::1234%3]:3339".parse().unwrap()),Some([0x2e,0x41,0xee,0x44,0xbd,0x03]));
    }

}
