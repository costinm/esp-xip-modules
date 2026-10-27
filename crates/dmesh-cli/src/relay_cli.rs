//! Flat disc relay command fields, encoded as numeric tagged CBOR.
fn bytes(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("hex:").unwrap_or(s);
    if s.len() % 2 != 0 || !s.is_ascii() {
        return Err("expected hex bytes".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "invalid hex bytes".into()))
        .collect()
}
fn dcid(s: &str) -> Result<Vec<u8>, String> {
    let b = bytes(s)?;
    if b.is_empty() || b.len() > 20 {
        Err("DCID must contain 1..20 bytes".into())
    } else {
        Ok(b)
    }
}
pub(super) fn encode(method: &str, words: &[String], id: u64) -> Result<Vec<u8>, String> {
    let (tag, allowed, required): (_, &[_], &[_]) = match method {
        "disc.relay_create" => (
            3,
            &[
                "transport",
                "dest",
                "rewritten_dcid",
                "weight",
                "endpoint",
                "idle_ms",
                "reuse",
            ],
            &["transport", "dest", "rewritten_dcid"],
        ),
        "disc.relay_add" => (
            4,
            &["dcid", "transport", "dest", "rewritten_dcid", "weight"],
            &["dcid", "transport", "dest", "rewritten_dcid"],
        ),
        "disc.relay_get" => (5, &["dcid"], &["dcid"]),
        "disc.relay_list" => (6, &["index"], &[]),
        "disc.relay_remove" => (7, &["dcid"], &["dcid"]),
        "disc.relay_remove_hop" => (
            8,
            &["dcid", "transport", "dest", "rewritten_dcid"],
            &["dcid", "transport", "dest", "rewritten_dcid"],
        ),
        "disc.relay_set_idle" => (9, &["dcid", "idle_ms"], &["dcid", "idle_ms"]),
        _ => return Err("unknown relay command".into()),
    };
    let mut fields = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < words.len() {
        let option = words[i]
            .strip_prefix("--")
            .ok_or("relay fields must start with --")?;
        let (key, value) = if let Some(v) = option.split_once('=') {
            v
        } else {
            i += 1;
            (
                option,
                words.get(i).ok_or("missing relay field value")?.as_str(),
            )
        };
        if !allowed.contains(&key) || fields.insert(key, value).is_some() {
            return Err(format!("unknown or duplicate relay field {key}"));
        }
        i += 1;
    }
    for key in required {
        if !fields.contains_key(key) {
            return Err(format!("{method} requires --{key}"));
        }
    }
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(4)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(47)
        .unwrap()
        .u8(2)
        .unwrap()
        .u8(tag)
        .unwrap()
        .u8(3)
        .unwrap()
        .u64(id)
        .unwrap()
        .u8(5)
        .unwrap()
        .map(fields.len() as u64)
        .unwrap();
    for (key, value) in &fields {
        match *key {
            "dcid" => {
                e.u8(1).unwrap().bytes(&dcid(value)?).unwrap();
            }
            "index" => {
                e.u8(1)
                    .unwrap()
                    .u32(value.parse().map_err(|_| "invalid index")?)
                    .unwrap();
            }
            "transport" => {
                if value.is_empty() || value.len() > 15 || value.contains('\0') || *value == "relay"
                {
                    return Err("invalid transport name".into());
                }
                e.u8(2).unwrap().str(value).unwrap();
            }
            "dest" => {
                e.u8(3).unwrap();
                if let Ok(token) = value.parse::<u64>() {
                    e.u64(token).unwrap();
                } else if fields
                    .get("transport")
                    .is_some_and(|t| t.starts_with("udp"))
                    && value.starts_with("udp://")
                {
                    let peer = crate::client::parse_udp_peer(value)?;
                    if peer.port() == 0 {
                        return Err("UDP destination port must be nonzero".into());
                    }
                    let (ip, scope) = match peer {
                        std::net::SocketAddr::V4(v) => (v.ip().to_ipv6_mapped(), 0),
                        std::net::SocketAddr::V6(v) => (*v.ip(), v.scope_id()),
                    };
                    let mut b = ip.octets().to_vec();
                    b.extend(peer.port().to_be_bytes());
                    b.extend(scope.to_be_bytes());
                    e.bytes(&b).unwrap();
                } else {
                    let b = bytes(&value.strip_prefix("hex:").unwrap_or(value).replace(':', ""))?;
                    if b.len() != 6 {
                        return Err("dest must be a link token, MAC or UDP address".into());
                    }
                    e.bytes(&b).unwrap();
                }
            }
            "rewritten_dcid" => {
                e.u8(4).unwrap().bytes(&dcid(value)?).unwrap();
            }
            "weight" => {
                e.u8(5)
                    .unwrap()
                    .u16(value.parse().map_err(|_| "weight must be 0..65535")?)
                    .unwrap();
            }
            "endpoint" => {
                e.u8(6)
                    .unwrap()
                    .bool(value.parse().map_err(|_| "invalid endpoint boolean")?)
                    .unwrap();
            }
            "idle_ms" => {
                e.u8(7)
                    .unwrap()
                    .u32(value.parse().map_err(|_| "invalid idle_ms")?)
                    .unwrap();
            }
            "reuse" => {
                e.u8(8)
                    .unwrap()
                    .bool(value.parse().map_err(|_| "invalid reuse boolean")?)
                    .unwrap();
            }
            _ => unreachable!(),
        }
    }
    Ok(e.into_writer())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flat_fields_encode_typed_cbor_without_json() {
        let b = encode(
            "disc.relay_create",
            &[
                "--transport=espnow".into(),
                "--dest=84:0d:8e:07:41:70".into(),
                "--rewritten_dcid=03".into(),
                "--idle_ms=1000".into(),
            ],
            9,
        )
        .unwrap();
        let v = crate::console::decode(&b).unwrap();
        assert_eq!(v["1"], 47);
        assert_eq!(v["5"]["3"], "hex:840d8e074170");
        assert_eq!(v["5"]["4"], "hex:03");
        assert_eq!(v["5"]["7"], 1000);
        assert!(encode("disc.relay_create", &["--dcid=21".into()], 1).is_err());
        assert!(encode(
            "disc.relay_add",
            &["--dcid=21".into(), "--hops=[]".into()],
            1
        )
        .is_err());
    }
}
