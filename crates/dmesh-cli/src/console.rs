//! Native module PPP control. Names are parsed on the host; only CBOR crosses UART.
use crate::xip_wire::{Event, PORT_QUIC, PORT_NAN_REPORT, Reader, XipPort, encode_record, unwrap_answer, wrap_request};
use mesh::tagged::TaggedSchema;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn schema() -> Result<TaggedSchema, String> {
    let mut tools = Vec::new();
    for text in [
        include_str!("../../../api/generated/schemas/xip-con/tools.json"),
        include_str!("../../../api/generated/schemas/xip-system/tools.json"),
        include_str!("../../../api/generated/schemas/xip-wifi/tools.json"),
        include_str!("../../../api/generated/schemas/xip-transport/tools.json"),
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
                    // Preserve declared strings even when they look numeric
                    // (notably SSIDs, passwords and NV keys).
                    if field.get("x-mesh-cbor-type").is_none() {
                        match field["x-protobuf-type"].as_str() {
                            Some("bytes") => field["x-mesh-cbor-type"] = json!("hex"),
                            Some("string") => field["x-mesh-cbor-type"] = json!("string"),
                            _ => {}
                        }
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
    let alias=method.strip_prefix("rtest.").map(|name|format!("test.{name}"));
    let method=alias.as_deref().unwrap_or(method);
    let method = if method.contains('.') {
        method.to_owned()
    } else {
        format!("console.{method}")
    };
    if method == "transport.set" {
        // The shared argv parser tolerates unknown fields. For transport
        // patches, dropping an old selector would acknowledge the wrong intent.
        let tools: Value=serde_json::from_str(include_str!("../../../api/generated/schemas/xip-transport/tools.json"))
            .map_err(|e|e.to_string())?;
        let fields=tools[0]["inputSchema"]["properties"].as_object().ok_or("invalid transport catalog")?;
        if words.len()<2 {return Err("transport.set needs at least one setting".into());}
        for word in &words[1..] {
            if let Some(option)=word.strip_prefix("--") {
                let key=option.split('=').next().unwrap_or(option);
                if !fields.contains_key(key) {return Err(format!("unsupported transport.set option --{key}"));}
            }
        }
    }
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
pub(crate) fn decode(bytes: &[u8]) -> Result<Value, String> {
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

/// Native C Main operations use the same target/method form as other CLI calls.
pub(crate) fn is_native(method: &str) -> bool {
    matches!(method,"wake"|"sleep"|"nv"|"watch"|"transport.set") || ["system.","nan.","wifi.","ble.","disc.","quic.","lora.","hw.","rtest.","test.","console."].iter().any(|p|method.starts_with(p))
}
fn native_words(words: &[String]) -> Result<Vec<String>,String> {
    match words.first().map(String::as_str) {
        Some("wake") if words.len()==2=>{
            let peer=crate::device::load_device(&words[1])?;
            let mac=peer.mac.ok_or("peer record has no radio MAC")?;
            Ok(vec!["nan.wake".into(),format!("--target=hex:{}",mac.iter().map(|b|format!("{b:02x}")).collect::<String>()),"--kind=6".into(),"--now=true".into()])
        },
        Some("sleep")=>{let mut out=vec!["nan.sleepy".into(),"--enabled=true".into(),"--immediate=true".into()];out.extend_from_slice(&words[1..]);Ok(out)},
        Some("nv")=>{
            if words.len()!=2 {return Err("usage: dmesh-cli DEVICE nv KEY[=VALUE]".into());}
            let (key,value)=words[1].split_once('=').map_or((words[1].as_str(),None),|(k,v)|(k,Some(v)));
            if key.is_empty() {return Err("NV key is empty".into());}
            let mut out=vec!["console.nv".into(),format!("--key={key}")];
            if let Some(value)=value {out.push(format!("--value=hex:{}",value.as_bytes().iter().map(|b|format!("{b:02x}")).collect::<String>()));}
            Ok(out)
        },
        _=>Ok(words.to_vec()),
    }
}
fn report(bytes: &[u8]) -> Result<Value,String> {
    use std::sync::OnceLock;
    static FIELDS: OnceLock<Result<Vec<(u64,String)>,String>>=OnceLock::new();
    let fields=FIELDS.get_or_init(|| {
        let tools:Value=serde_json::from_str(include_str!("../../../api/generated/schemas/xip-nan/tools.json")).map_err(|e|e.to_string())?;
        let properties=tools.as_array().and_then(|tools|tools.iter().find(|t|t["name"]=="nan.report"))
            .and_then(|t|t.pointer("/outputSchema/properties")).and_then(Value::as_object)
            .ok_or("generated nan.report schema is missing")?;
        Ok(properties.iter().filter_map(|(name,field)|Some((field["x-protobuf-index"].as_u64()?,name.clone()))).collect())
    }).as_ref().map_err(Clone::clone)?;
    let raw=decode(bytes)?;
    let kind=raw["1"].as_u64().ok_or("report has no kind")?;
    let event=match kind {0=>"nan.dw0",1=>"nan.pub",2=>"nan.sub",3=>"nan.followup",4=>"nan.now",_=>return Err("unknown NAN report".into())};
    let mut out=serde_json::Map::new();
    out.insert("event".into(),json!(event));
    for (tag,name) in fields {
        if name=="kind" {continue;}
        if let Some(value)=raw.get(tag.to_string()) {out.insert(name.clone(),value.clone());}
    }
    Ok(Value::Object(out))
}
fn watch(owner: &mut XipPort) -> Result<(),String> {
    let mut reader=Reader::default();
    loop {
        let bytes=owner.read().map_err(|e|e.to_string())?;
        for event in reader.feed(&bytes) {
            if let Event::Record(PORT_NAN_REPORT,data)=event {
                if let Ok(report)=report(&data) {println!("{report}");}
            }
        }
        let text=reader.take_text();
        if !text.is_empty() {eprint!("{}",String::from_utf8_lossy(&text));}
        if bytes.is_empty() {std::thread::sleep(Duration::from_millis(5));}
    }
}
pub(crate) fn run_native(arguments: &[String]) -> Result<(),String> {
    let target=arguments.first().ok_or("missing device")?;
    if target.starts_with("udp://") {
        let words=native_words(&arguments[1..])?;
        let req=request(&words,crate::client::fresh_request_id()&i64::MAX as u64)?;
        return run_udp(target,&req);
    }
    let (path,default_baud)=if target.starts_with('/') {(target.clone(),None)} else {
        let profile=crate::device::load_device(target)?;
        (profile.serial_path()?.ok_or_else(||format!("device {target:?} has no UART companion path"))?.display().to_string(),profile.uart_baud)
    };
    let mut words=Vec::new();let mut baud=default_baud;let mut i=1;
    while i<arguments.len() {
        if arguments[i]=="--baud" {i+=1;baud=Some(arguments.get(i).ok_or("missing --baud")?.parse::<u32>().map_err(|e|e.to_string())?);}
        else {words.push(arguments[i].clone());}
        i+=1;
    }
    let waking=words.as_slice()==["wake"];
    let watching=words.as_slice()==["watch"];
    let words=native_words(&words)?;
    // Encode/validate before touching the physical port.
    let req=if waking || watching {None} else {Some(request(&words,crate::client::fresh_request_id())?)};
    let mut owner=XipPort::open_baud(path,baud).map_err(|e|e.to_string())?;
    if watching {return watch(&mut owner);}
    owner.wake()?;
    let Some(req)=req else {println!("{}",json!({"awake":true}));return Ok(());};
    run_request(&mut owner,PORT_QUIC,&req,Duration::from_secs(6))
}
fn run_udp(target:&str,request:&Request)->Result<(),String>{
    let peer=crate::client::parse_udp_peer(target)?;
    let socket=std::net::UdpSocket::bind(quic_lite::bearer_udp::wildcard_bind(peer,0)).map_err(|e|e.to_string())?;
    socket.connect(peer).map_err(|e|e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(6))).map_err(|e|e.to_string())?;
    let packet=wrap_request(&request.bytes);let mut reader=Reader::default();let mut buf=[0u8;1704];
    for _ in 0..3 {
        socket.send(&packet).map_err(|e|e.to_string())?;
        loop {match socket.recv(&mut buf){
            Ok(n)=>if let Some(answer)=response(&mut reader,&encode_record(PORT_QUIC,&buf[..n]),PORT_QUIC,request)? {println!("{answer}");return Ok(())},
            Err(e) if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut)=>break,
            Err(e)=>return Err(e.to_string()),
        }}
    }
    Err(format!("native method {}.{} did not reply",request.component,request.method))
}
fn run_request(owner: &mut XipPort, port: u32, request: &Request, timeout: Duration) -> Result<(),String> {
    owner.write_all(&encode_record(port,&wrap_request(&request.bytes))).map_err(|e|e.to_string())?;
    let mut reader=Reader::default();let end=Instant::now()+timeout;
    while Instant::now()<end {
        let chunk=owner.read().map_err(|e|e.to_string())?;
        if let Some(answer)=response(&mut reader,&chunk,port,request)? {println!("{answer}");return Ok(());}
        if chunk.is_empty() {std::thread::sleep(Duration::from_millis(5));}
    }
    Err(format!("native method {}.{} did not reply",request.component,request.method))
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
    if path.starts_with("udp://") {return run_udp(path,&request);}
    let mut owner = XipPort::open(path).map_err(|e| e.to_string())?;
    owner.wake()?;
    run_request(&mut owner,port,&request,Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_wifi_preserves_numeric_strings_and_boolean_intent() {
        let req=request(&["wifi.set".into(),"--radio=true".into(),
            "--sta=true".into(),"--now=false".into(),"--sta_ssid=00123".into(),
            "--sta_psk=0123456789".into()],1).unwrap();
        let v=decode(&req.bytes).unwrap();
        assert_eq!(v["5"]["6"],"00123");
        assert_eq!(v["5"]["7"],"0123456789");
        assert_eq!(v["5"]["2"],true);
        assert_eq!(v["5"]["10"],false);
    }
    #[test]
    fn native_transport_uses_current_runtime_wire_tags() {
        let req=request(&["transport.set".into(),"--sta=0".into(),"--uart=1".into(),"--nan_dw_interval=8".into()],1).unwrap();
        let v=decode(&req.bytes).unwrap();
        assert_eq!((req.component,req.method),(1,4));
        assert_eq!(v["5"]["18"],1);assert_eq!(v["5"]["14"],8);assert_eq!(v["5"]["1"],0);
        assert!(request(&["transport.set".into(),"--mode=6".into()],2).is_err());
        assert!(request(&["transport.set".into(),"--wake_target=hex:001122334455".into()],3).is_err());
    }
    #[test]
    fn power_methods_belong_to_nan_in_the_public_catalog() {
        for (name,method) in [("nan.powerinfo",13),("nan.powersleep",14),("nan.dump_locks",15)] {
            let req=request(&[name.into()],1).unwrap();
            assert_eq!((req.component,req.method),(46,method));
        }
        assert!(!is_native("power.info"));assert!(!is_native("power.sleep"));
    }
    #[test]
    fn system_diagnostics_have_one_owner_and_retired_names_are_absent() {
        for (name,method) in [("system.heap",1),("system.heap_dump",2),("system.dump_tasks",3)] {
            let req=request(&[name.into()],1).unwrap();assert_eq!((req.component,req.method),(49,method));
        }
        assert!(request(&["nan.dump_tasks".into()],1).is_err());
        assert!(request(&["console.heap".into()],1).is_err());
    }
    #[test]
    fn periodic_reports_decode_to_user_facing_fields() {
        let bytes=[0xa5,1,0,2,0x43,1,2,3,3,9,4,18,5,0xf5];
        let value=report(&bytes).unwrap();
        assert_eq!(value["event"],"nan.dw0");assert_eq!(value["cluster"],"hex:010203");
        assert_eq!(value["first_rx_ms"],9);assert_eq!(value["sync_ms"],18);assert_eq!(value["now_cluster"],true);
    }
    #[test]
    fn shortcuts_need_no_console_port_or_timing_arguments() {
        let req=request(&native_words(&["sleep".into()]).unwrap(),1).unwrap();
        let v=decode(&req.bytes).unwrap();assert_eq!(v["5"]["1"],true);assert_eq!(v["5"]["4"],true);
        assert!(v["5"].get("3").is_none());
        let req=request(&native_words(&["nv".into(),"cadence=8".into()]).unwrap(),2).unwrap();
        let v=decode(&req.bytes).unwrap();assert_eq!(v["5"]["1"],"cadence");assert_eq!(v["5"]["2"],"hex:38");
        assert!(is_native("nan.status"));assert!(is_native("wake"));assert!(!is_native("probe.handler"));
    }
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

/// Canonical native envelope for the bidirectional NAN companion connection.
pub(crate) fn stream_request(words:&[String],id:u64)->Result<Vec<u8>,String>{request(words,id).map(|r|r.bytes)}
