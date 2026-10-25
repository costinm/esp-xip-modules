//! Native module PPP control. Names are parsed on the host; only CBOR crosses UART.
use crate::xip_wire::{Event, PORT_QUIC, Reader, XipPort, encode_record, unwrap_answer, wrap_request};
use mesh::tagged::TaggedSchema;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn schema() -> Result<TaggedSchema, String> {
    let mut tools = Vec::new();
    for text in [
        include_str!("../../../api/generated/schemas/xip-con/tools.json"),
        include_str!("../../../api/generated/schemas/xip-wifi/tools.json"),
        include_str!("../../../api/generated/schemas/xip-power/tools.json"),
        include_str!("../../../api/generated/schemas/xip-ip/tools.json"),
        include_str!("../../../api/generated/schemas/xip-ble/tools.json"),
        include_str!("../../../api/generated/schemas/xip-nan/tools.json"),
        include_str!("../../../api/generated/schemas/xip-disc/tools.json"),
        include_str!("../../../api/generated/schemas/xip-quic/tools.json"),
        include_str!("../../../api/generated/schemas/xip-lora/tools.json"),
        include_str!("../../../api/generated/schemas/xip-hw/tools.json"),
        include_str!("../../../api/generated/schemas/xip-rtest/tools.json"),
    ] {
        let mut entries: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
        for entry in &mut entries {
            if let Some(fields) = entry
                .pointer_mut("/inputSchema/properties")
                .and_then(Value::as_object_mut)
            {
                for field in fields.values_mut() {
                    if field["x-protobuf-type"] == "bytes" {
                        field["x-mesh-cbor-type"] = json!("hex");
                    }
                }
            }
        }
        tools.extend(entries);
    }
    TaggedSchema::from_tools_json(&Value::Array(tools)).map_err(|e| e.to_string())
}

struct Request {
    bytes: Vec<u8>,
    component: u64,
    method: u64,
    id: u64,
}

fn request(words: &[String], id: u64) -> Result<Request, String> {
    let method = words.first().map_or("console.status", String::as_str);
    let method = if method.contains('.') {
        method.to_owned()
    } else {
        format!("console.{method}")
    };
    let schema = schema()?;
    let mut record = schema
        .parse_argv(&method, words.get(1..).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    record.id = Some(Value::from(id));
    let bytes = schema.encode_request(&record).map_err(|e| e.to_string())?;
    let decoded = decode(&bytes)?;
    Ok(Request {
        component: decoded["1"].as_u64().ok_or("missing component")?,
        method: decoded["2"].as_u64().ok_or("missing method")?,
        bytes,
        id,
    })
}

/// Keep unknown/nested response fields structured, without guessing names from request fields.
fn value(d: &mut minicbor::Decoder<'_>, depth: usize) -> Result<Value, String> {
    use minicbor::data::Type;
    if depth > 32 {
        return Err("CBOR response nesting exceeds 32".into());
    }
    let err = |e: minicbor::decode::Error| e.to_string();
    Ok(match d.datatype().map_err(err)? {
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => Value::from(d.u64().map_err(err)?),
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::Int => {
            Value::from(d.i64().map_err(err)?)
        }
        Type::Bool => Value::from(d.bool().map_err(err)?),
        Type::Null => {
            d.null().map_err(err)?;
            Value::Null
        }
        Type::String => Value::from(d.str().map_err(err)?),
        Type::Bytes => Value::from(format!(
            "hex:{}",
            d.bytes()
                .map_err(err)?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )),
        Type::Array => {
            let n = d
                .array()
                .map_err(err)?
                .ok_or("indefinite CBOR arrays are unsupported")?;
            let mut out = Vec::new();
            for _ in 0..n {
                out.push(value(d, depth + 1)?);
            }
            Value::Array(out)
        }
        Type::Map => {
            let n = d
                .map()
                .map_err(err)?
                .ok_or("indefinite CBOR maps are unsupported")?;
            let mut out = serde_json::Map::new();
            for _ in 0..n {
                let key = match d.datatype().map_err(err)? {
                    Type::String => d.str().map_err(err)?.to_owned(),
                    _ => d.u64().map_err(err)?.to_string(),
                };
                if out.insert(key, value(d, depth + 1)?).is_some() {
                    return Err("duplicate CBOR map key".into());
                }
            }
            Value::Object(out)
        }
        kind => return Err(format!("unsupported CBOR response type {kind:?}")),
    })
}
fn decode(bytes: &[u8]) -> Result<Value, String> {
    let mut d = minicbor::Decoder::new(bytes);
    let v = value(&mut d, 0)?;
    if d.position() != bytes.len() {
        return Err("trailing bytes in CBOR record".into());
    }
    Ok(v)
}

fn response(
    reader: &mut Reader,
    bytes: &[u8],
    port: u32,
    request: &Request,
) -> Result<Option<Value>, String> {
    for event in reader.feed(bytes) {
        let Event::Record(from, bytes) = event else {
            continue;
        };
        if from != port {
            continue;
        }
        // the device answers with a 0x444d0002 long packet around the envelope
        let Some(envelope) = unwrap_answer(&bytes) else { continue };
        let Ok(v) = decode(envelope) else { continue };
        if v["1"].as_u64() != Some(request.component)
            || v["2"].as_u64() != Some(request.method)
            || v["3"].as_u64() != Some(request.id)
        {
            continue;
        }
        if let Some(error) = v.get("7") {
            return Err(format!(
                "native method {}.{} failed: {error}",
                request.component, request.method
            ));
        }
        let result = v
            .get("6")
            .ok_or("correlated response has no result or error")?;
        return Ok(Some(
            json!({"component":request.component,"method":request.method,"id":request.id,"result":result}),
        ));
    }
    // Native platform boot/log text is not a console response. Discard it here;
    // --watch remains the explicit way to inspect those diagnostics.
    reader.take_text();
    Ok(None)
}

pub(crate) fn run(arguments: &[String]) -> Result<(), String> {
    let path=arguments.get(1).ok_or("usage: dmesh-cli console SERIAL [console.status|METHOD] [--field=value ...] [--port PPP_PORT (default 247, the QUIC port)] [--seconds N]")?;
    let (mut words, mut port, mut seconds) = (Vec::new(), PORT_QUIC, 6.0f64);
    let mut i = 2;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "--port" => {
                i += 1;
                port = arguments
                    .get(i)
                    .ok_or("--port needs a value")?
                    .parse::<u32>()
                    .map_err(|e| e.to_string())?;
            }
            "--seconds" => {
                i += 1;
                seconds = arguments
                    .get(i)
                    .ok_or("--seconds needs a value")?
                    .parse::<f64>()
                    .map_err(|e| e.to_string())?;
            }
            word => words.push(word.to_owned()),
        }
        i += 1;
    }
    if port > 255 {
        return Err("--port must be 0..255".into());
    }
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 300.0 {
        return Err("--seconds must be greater than zero and at most 300".into());
    }
    let request = request(&words, crate::client::fresh_request_id())?;
    let mut owner = XipPort::open(path).map_err(|e| e.to_string())?;
    owner
        .write_all(&encode_record(port, &wrap_request(&request.bytes)))
        .map_err(|e| e.to_string())?;
    let mut reader = Reader::default();
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < end {
        let chunk = owner.read().map_err(|e| e.to_string())?;
        if let Some(answer) = response(&mut reader, &chunk, port, &request)? {
            println!("{answer}");
            return Ok(());
        }
        if chunk.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Err(format!(
        "native method {}.{} timed out after {seconds}s",
        request.component, request.method
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_commands_encode_bytes_and_numeric_envelopes() {
        let req = request(
            &[
                "console.start".into(),
                "--name=flash".into(),
                "--request=hex:756e7369676e6564".into(),
            ],
            0x1_0000_0042,
        )
        .unwrap();
        let v = decode(&req.bytes).unwrap();
        assert_eq!(
            (v["1"].as_u64(), v["2"].as_u64(), v["3"].as_u64()),
            (Some(20), Some(6), Some(0x1_0000_0042))
        );
        assert_eq!(v["5"]["1"], "flash");
        assert_eq!(v["5"]["2"], "hex:756e7369676e6564");
        let v = decode(&request(&["nan.set_announce".into(), "--record=hex:007e7d".into()], 9).unwrap().bytes).unwrap();
        assert_eq!((v["1"].as_u64(), v["2"].as_u64()), (Some(46),Some(11)));
        assert_eq!(v["5"]["1"], "hex:007e7d");
        let v = decode(&request(&["disc.publish".into(), "--refresh=true".into()], 10).unwrap().bytes).unwrap();
        assert_eq!((v["1"].as_u64(), v["2"].as_u64()), (Some(47),Some(2)));
        assert_eq!(v["5"]["1"], true);
        assert_eq!(request(&[], 1).unwrap().method, 5);
        assert!(request(&["parse".into()], 1).is_err());
        assert!(request(&["render".into()], 1).is_err());
        assert!(request(&["start".into(), "flash".into()], 1).is_err());
    }
    fn reply(component: u64, method: u64, id: u64, error: bool) -> Vec<u8> {
        let mut e = minicbor::Encoder::new(Vec::new());
        e.map(4)
            .unwrap()
            .u8(1)
            .unwrap()
            .u64(component)
            .unwrap()
            .u8(2)
            .unwrap()
            .u64(method)
            .unwrap()
            .u8(3)
            .unwrap()
            .u64(id)
            .unwrap();
        if error {
            e.u8(7).unwrap().i8(-2).unwrap();
        } else {
            e.u8(6)
                .unwrap()
                .map(1)
                .unwrap()
                .u8(4)
                .unwrap()
                .array(1)
                .unwrap()
                .map(4)
                .unwrap()
                .u8(1)
                .unwrap()
                .str("nan")
                .unwrap()
                .u8(5)
                .unwrap()
                .u8(3)
                .unwrap()
                .u8(6)
                .unwrap()
                .i8(-43)
                .unwrap()
                .u8(8)
                .unwrap()
                .bytes(&[2, 0, 0x7e, 0x7d, 0, 1])
                .unwrap();
        }
        e.into_writer()
    }
    /// What the device sends back for a request: the reply envelope in a 0x444d0002 long packet, our CIDs swapped.
    fn answer(envelope: Vec<u8>) -> Vec<u8> {
        crate::xip_wire::long_packet(crate::xip_wire::VERSION_CBOR, &[2], &[1], &envelope)
    }
    #[test]
    fn native_response_matches_port_component_method_and_full_id() {
        let req = request(&[], 0x1_0000_0042).unwrap();
        let mut r = Reader::default();
        for (port, c, m, id) in [
            (2, 20, 5, req.id),
            (1, 20, 6, req.id),
            (1, 10, 5, req.id),
            (1, 20, 5, 0x42),
        ] {
            assert!(
                response(
                    &mut r,
                    &encode_record(port, &answer(reply(c, m, id, false))),
                    1,
                    &req
                )
                .unwrap()
                .is_none()
            );
        }
        assert!(response(&mut r, b"status\n", 1, &req).unwrap().is_none());
        let wire = encode_record(1, &answer(reply(20, 5, req.id, false)));
        let middle = wire.len() / 2;
        assert!(
            response(&mut r, &wire[..middle], 1, &req)
                .unwrap()
                .is_none()
        );
        let v = response(&mut r, &wire[middle..], 1, &req).unwrap().unwrap();
        assert_eq!(v["result"]["4"][0]["1"], "nan");
        assert_eq!(v["result"]["4"][0]["5"], 3);
        assert_eq!(v["result"]["4"][0]["6"], -43);
        assert_eq!(v["result"]["4"][0]["8"], "hex:02007e7d0001");
        assert!(
            response(
                &mut r,
                &encode_record(1, &answer(reply(20, 5, req.id, true))),
                1,
                &req
            )
            .unwrap_err()
            .contains("-2")
        );
    }
}
