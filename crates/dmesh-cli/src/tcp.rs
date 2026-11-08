//! Direct module streams over TCP, without a QUIC or console module.
use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs, UdpSocket},
    time::Duration,
};
fn tool(name: &str) -> Result<serde_json::Value, String> {
    let tools: serde_json::Value = serde_json::from_str(include_str!(
        "../../../api/generated/schemas/xip-flash/tools.json"
    ))
    .map_err(|e| e.to_string())?;
    tools
        .as_array()
        .and_then(|tools| {
            tools
                .iter()
                .find(|t| t["name"] == name && t["x-method-index"].as_u64().is_some_and(|m| m >= 2))
        })
        .cloned()
        .ok_or_else(|| format!("unknown flash stream method {name}"))
}
fn recovery_announcement(data: &[u8]) -> Option<serde_json::Value> {
    // Receive compatibility for boards not migrated yet; new firmware emits CBOR only.
    if let Ok(text)=std::str::from_utf8(data) {
        if text.starts_with("esp-wifi-loader v=1 ") {
            let mut value=serde_json::Map::new();
            for field in text.split_whitespace() {
                if let Some((k,v))=field.split_once('=') { value.insert(k.into(),v.into()); }
            }
            return Some(value.into());
        }
    }
    let mut d=minicbor::Decoder::new(data);
    let count=d.map().ok()??;
    let (mut component,mut format,mut mac)=(None,None,None);
    let mut value=serde_json::Map::new();
    for _ in 0..count {
        match d.u32().ok()? {
            0=>component=Some(d.u32().ok()?),
            1=>format=Some(d.u32().ok()?),
            2=>{value.insert("version".into(),d.u32().ok()?.into());},
            3=>{let b=d.bytes().ok()?; if b.len()!=6{return None;} mac=Some(b.iter().map(|x|format!("{x:02x}")).collect::<Vec<_>>().join(":"));},
            4=>{value.insert("port".into(),d.u16().ok()?.into());},
            5=>{let chip=match d.u32().ok()? {1=>"esp32",2=>"esp32s3",3=>"esp32c6",_=>return None};value.insert("chip".into(),chip.into());},
            6=>{value.insert("role".into(),if d.bool().ok()? {"sta"} else {"ap"}.into());},
            7=>{value.insert("up".into(),d.u64().ok()?.into());},
            _=>d.skip().ok()?,
        }
    }
    if component!=Some(44) || format!=Some(1) || d.position()!=data.len(){return None;}
    value.insert("mac".into(),mac?.into());
    value.insert("mode".into(),"main".into());
    Some(value.into())
}
pub(crate) fn discover(target: &str) -> Result<(), String> {
    if target.starts_with("/dev/") {
        use crate::xip_wire::{encode_record, Event, Reader, XipPort};
        let mut port = XipPort::open(target.to_owned()).map_err(|e| e.to_string())?;
        port.wake()?;
        let mut next = std::time::Instant::now();
        let until = std::time::Instant::now() + Duration::from_secs(6);
        let mut reader = Reader::default();
        while std::time::Instant::now() < until {
            if std::time::Instant::now() >= next {
                port.write_all(&encode_record(250, b"discover"))
                    .map_err(|e| e.to_string())?;
                next = std::time::Instant::now() + Duration::from_millis(250);
            }
            let bytes = port.read().map_err(|e| e.to_string())?;
            for event in reader.feed(&bytes) {
                if let Event::Record(250, data) = event {
                    if let Some(record) = recovery_announcement(&data) {
                        crate::inventory::Inventory::from_env()?.observe_recovery(&record, target)?;
                        println!("PPP port=250 {record}");
                        return Ok(());
                    }
                }
            }
            let text = reader.take_text();
            if !text.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&text));
            }
            if bytes.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        return Err("no recovery announcement over PPP port 250".into());
    }

    // An interface name actively discovers peers; a HOST:PORT probes one peer.
    let multicast = if target.contains(':') {
        None
    } else {
        Some(multicast_target(target)?)
    };
    let socket = UdpSocket::bind(if multicast.is_some() || target.starts_with('[') {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|e| e.to_string())?;
    let until = std::time::Instant::now() + Duration::from_secs(5);
    let mut buf = [0; 1024];
    while std::time::Instant::now() < until {
        if let Some(peer) = multicast {
            socket
                .send_to(b"discover", peer)
                .map_err(|e| e.to_string())?;
        } else {
            socket
                .send_to(b"discover", target)
                .map_err(|e| e.to_string())?;
        }
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(reply) => reply,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some(record) = recovery_announcement(&buf[..n]) else { continue; };
        crate::inventory::Inventory::from_env()?.observe_recovery(&record, &src.to_string())?;
        println!("source={src} {record}");
        return Ok(());
    }
    Err("no flash discovery reply within 5 seconds".into())
}

pub(crate) fn run(args: &[String]) -> Result<(), String> { run_mode(args,true) }
pub(crate) fn run_quiet(args: &[String]) -> Result<(), String> { run_mode(args,false) }
fn run_mode(args: &[String], report: bool) -> Result<(), String> {
    if args
        .get(1)
        .is_some_and(|m| matches!(m.as_str(), "flash.module.write" | "flash.runtime.write"))
    {
        run_image(args, Duration::ZERO, report)
    } else {
        run_wait(args, Duration::ZERO, report)
    }
}
fn run_image(args: &[String], wait: Duration, report: bool) -> Result<(), String> {
    // Image identity/version and operation id make a complete retransmission safe,
    // including when the commit succeeded but the response was lost.
    for attempt in 1..=3 {
        match run_wait(args, wait, report) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let transport = error.starts_with("TCP ")
                    || error.starts_with("response header:")
                    || error.contains("Broken pipe")
                    || error.contains("Connection reset")
                    || error.contains("timed out");
                if !transport || attempt == 3 {
                    return Err(error);
                }
                eprintln!(
                    "image transfer interrupted; retrying the same image operation ({attempt}/3)"
                );
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    unreachable!()
}
fn run_wait(args: &[String], wait: Duration, report: bool) -> Result<(), String> {
    let [target, operation, rest @ ..] = args else {
        return Err("usage: dmesh-cli tcp://HOST:5227 flash.METHOD [--name=NAME] [--input=PATH] [--output=PATH] [--size=N] [--operation_id=N] [--version=N]".into());
    };
    let tool = tool(operation)?;
    let method = tool["x-method-index"]
        .as_u64()
        .ok_or("invalid method catalog")? as u32;
    let properties = tool["inputSchema"]["properties"]
        .as_object()
        .ok_or("invalid field catalog")?;
    let mut input = None;
    let mut output = None;
    let mut chunk = 8192usize;
    let mut chunk_delay_ms=0u64;
    let mut fields = Vec::new();
    for arg in rest {
        let (k, v) = arg
            .strip_prefix("--")
            .and_then(|s| s.split_once('='))
            .ok_or("expected --field=value")?;
        match k {
            "input" => input = Some(v),
            "output" => output = Some(v),
            "chunk" => chunk = v.parse().map_err(|_| "invalid chunk")?,
            "chunk-delay-ms" => chunk_delay_ms=v.parse().map_err(|_| "invalid chunk-delay-ms")?,
            _ => fields.push((k, v)),
        }
    }
    if chunk_delay_ms>1000 { return Err("chunk-delay-ms must be 0..1000".into()); }
    if chunk == 0 || chunk > 65536 {
        return Err("chunk must be 1..65536".into());
    }
    let mut file = input
        .map(File::open)
        .transpose()
        .map_err(|e| e.to_string())?;
    let auto_size = file
        .as_ref()
        .map(|f| f.metadata().map(|m| m.len()))
        .transpose()
        .map_err(|e| e.to_string())?;
    if let (Some(size), Some(actual)) = (
        fields.iter().find(|(k, _)| *k == "size").map(|(_, v)| *v),
        auto_size,
    ) {
        if size.parse::<u64>().map_err(|_| "invalid size")? != actual {
            return Err("size must equal the input file length".into());
        }
    }
    let add_size = auto_size.is_some() && !fields.iter().any(|(k, _)| *k == "size");
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(4)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(44)
        .unwrap()
        .u8(2)
        .unwrap()
        .u32(method)
        .unwrap()
        .u8(3)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(5)
        .unwrap()
        .map((fields.len() + usize::from(add_size)) as u64)
        .unwrap();
    for (k, v) in fields {
        let field = properties
            .get(k)
            .ok_or_else(|| format!("unknown field {k} for {operation}"))?;
        let tag = field["x-protobuf-index"]
            .as_u64()
            .ok_or("invalid field tag")?;
        e.u64(tag).unwrap();
        match field["x-protobuf-type"].as_str() {
            Some("bytes") => {
                let bytes = if let Some(hex) = v.strip_prefix("hex:") {
                    if hex.len() % 2 != 0 {
                        return Err("odd hex byte string".into());
                    }
                    (0..hex.len())
                        .step_by(2)
                        .map(|i| {
                            hex.get(i..i + 2)
                                .and_then(|s| u8::from_str_radix(s, 16).ok())
                                .ok_or("invalid hex byte string")
                        })
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    v.as_bytes().to_vec()
                };
                e.bytes(&bytes).unwrap();
            }
            Some("bool") => {
                e.bool(match v {
                    "true" => true,
                    "false" => false,
                    _ => return Err(format!("{k} needs true or false")),
                })
                .unwrap();
            }
            Some("u32" | "u64" | "u16" | "u8") => {
                e.u64(
                    v.parse()
                        .map_err(|_| format!("{k} needs an unsigned integer"))?,
                )
                .unwrap();
            }
            _ => return Err(format!("unsupported field type for {k}")),
        }
    }
    if add_size && !properties.contains_key("size") {
        return Err(format!("{operation} has no stream body"));
    }
    if add_size {
        e.u8(2).unwrap().u64(auto_size.unwrap()).unwrap();
    }
    let address = target.strip_prefix("tcp://").ok_or("expected tcp://")?;
    let mut last = None;
    let mut connected = None;
    let peers = address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .collect::<Vec<_>>();
    let deadline = std::time::Instant::now() + wait;
    loop {
        for peer in &peers {
            match TcpStream::connect_timeout(peer, Duration::from_secs(5)) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last = Some(error),
            }
        }
        if connected.is_some() || peers.is_empty() || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut stream = connected.ok_or_else(|| {
        format!(
            "TCP {address}: {}",
            last.map_or_else(|| "no resolved address".into(), |e| e.to_string())
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    for b in e.into_writer().chunks(chunk) {
        stream.write_all(b).map_err(|e| e.to_string())?;
    }
    let mut buf = vec![0; chunk];
    let mut sent = 0u64;
    if let Some(file) = file.as_mut() {
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            stream.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            sent += n as u64;
            if chunk_delay_ms!=0 {std::thread::sleep(Duration::from_millis(chunk_delay_ms));}
        }
    }
    let mut header = Vec::new();
    loop {
        let mut b = [0];
        stream
            .read_exact(&mut b)
            .map_err(|e| format!("response header: {e}"))?;
        header.push(b[0]);
        let mut d = minicbor::Decoder::new(&header);
        if d.skip().is_ok() && d.position() == header.len() {
            break;
        }
        if header.len() == 4096 {
            return Err("response metadata exceeds 4096 bytes".into());
        }
    }
    let meta = crate::console::decode(&header)?;
    if let Some(error) = meta.get("7").and_then(|v| v.as_i64()).filter(|e| *e < 0) {
        return Err(format!("flash error {error}"));
    }
    let mut out = output
        .map(File::create)
        .transpose()
        .map_err(|e| e.to_string())?;
    let received = if let Some(out) = out.as_mut() {
        std::io::copy(&mut stream, out)
    } else {
        std::io::copy(&mut stream, &mut std::io::sink())
    }
    .map_err(|e| e.to_string())?;
    if method == 3 && meta.get("5").and_then(|v| v.as_u64()) != Some(received) {
        return Err(format!("truncated read: received {received}"));
    }
    let hex = header
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if report {
    println!(
        "{}",
        serde_json::json!({"method":operation,"metadata":meta,"metadata_hex":hex,"sent":sent,"received":received})
    );
    }
    Ok(())
}

/// Passive recovery announcements prove the periodic multicast path.
pub(crate) fn listen(interface: &str, mac: Option<&str>) -> Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let name = std::ffi::CString::new(interface).map_err(|e| e.to_string())?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(format!("unknown interface {interface}"));
    }
    let raw = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let yes = 1i32;
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&yes as *const i32).cast(),
            4,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut address: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    address.sin6_family = libc::AF_INET6 as u16;
    address.sin6_port = 5227u16.to_be();
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_in6).cast(),
            std::mem::size_of_val(&address) as _,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let socket = UdpSocket::from(fd);
    socket
        .join_multicast_v6(&"ff02::5227".parse::<std::net::Ipv6Addr>().unwrap(), index)
        .map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(12)))
        .map_err(|e| e.to_string())?;
    let mut buf = [0; 2048];
    let until = std::time::Instant::now() + Duration::from_secs(12);
    while std::time::Instant::now() < until {
        let (n, src) = socket.recv_from(&mut buf).map_err(|e| e.to_string())?;
        if let Some(record) = recovery_announcement(&buf[..n]) {
            if mac.is_none_or(|mac| record["mac"].as_str().is_some_and(|actual| actual.eq_ignore_ascii_case(mac))) {
                crate::inventory::Inventory::from_env()?.observe_recovery(&record, &src.to_string())?;
                println!("multicast source={src} {record}");
                return Ok(());
            }
        }
    }
    Err("no periodic recovery multicast announcement".into())
}

fn multicast_target(interface: &str) -> Result<std::net::SocketAddrV6, String> {
    let name = std::ffi::CString::new(interface).map_err(|e| e.to_string())?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(format!("unknown network interface {interface}"));
    }
    Ok(std::net::SocketAddrV6::new(
        "ff02::5227".parse().unwrap(),
        5227,
        0,
        index,
    ))
}
#[cfg(test)]
mod discovery_tests {
    #[test]
    fn cbor_recovery_record_decodes_and_rejects_trailing_bytes() {
        let mut e=minicbor::Encoder::new(Vec::new());
        e.map(8).unwrap().u8(0).unwrap().u8(44).unwrap().u8(1).unwrap().u8(1).unwrap()
         .u8(2).unwrap().u32(123).unwrap().u8(3).unwrap().bytes(&[2,0,0,0,0,1]).unwrap()
         .u8(4).unwrap().u16(5227).unwrap().u8(5).unwrap().u8(3).unwrap()
         .u8(6).unwrap().bool(true).unwrap().u8(7).unwrap().u64(99).unwrap();
        let mut bytes=e.into_writer();
        let value=super::recovery_announcement(&bytes).unwrap();
        assert_eq!(value["mac"],"02:00:00:00:00:01");assert_eq!(value["chip"],"esp32c6");
        assert_eq!(value["version"],123);assert_eq!(value["role"],"sta");
        bytes.push(0);assert!(super::recovery_announcement(&bytes).is_none());
        assert!(super::recovery_announcement(&[0xa1,0,44]).is_none());
    }
    #[test]
    fn active_probe_retries_while_the_device_is_starting() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut buf = [0; 64];
            for attempt in 0..3 {
                let (n, source) = socket.recv_from(&mut buf).unwrap();
                assert_eq!(&buf[..n], b"discover");
                if attempt == 2 {
                    socket
                        .send_to(b"esp-wifi-loader v=1 mac=02:00:00:00:00:01", source)
                        .unwrap();
                }
            }
        });
        super::discover(&address.to_string()).unwrap();
        peer.join().unwrap();
    }
    #[test]
    fn interface_probe_uses_scoped_loader_group() {
        let peer = super::multicast_target("lo").unwrap();
        assert_eq!(peer.ip().to_string(), "ff02::5227");
        assert_eq!(peer.port(), 5227);
        assert!(peer.scope_id() > 0);
        assert!(super::multicast_target("no-such-flash-interface").is_err());
    }
}

/// One serial owner watches logs, waits for the recovery response, then optionally
/// uploads over TCP while continuing to drain the same console.
pub(crate) fn wifi(args: &[String]) -> Result<(), String> {
    use crate::xip_wire::{encode_record, Event, Reader, XipPort};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let [target, operation, rest @ ..] = args else {
        return Err("expected SERIAL flash.wifi.join|info".into());
    };
    let split = rest.iter().position(|a| a == "--").unwrap_or(rest.len());
    let tail = rest.get(split + 1..).unwrap_or(&[]);
    if !tail.is_empty()
        && !matches!(
            tail[0].as_str(),
            "flash.module.write" | "flash.runtime.write"
        )
    {
        return Err("after -- use flash.module.write or flash.runtime.write".into());
    }
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_micros() as u64;
    let request = wifi_request(operation, &rest[..split], id)?;
    let (serial, baud) = if target.starts_with('/') {
        (target.clone(), None)
    } else {
        let profile = crate::device::load_device(target)?;
        (
            profile
                .serial_path()?
                .ok_or("device has no serial path")?
                .display()
                .to_string(),
            profile.uart_baud,
        )
    };
    let mut port = XipPort::open_baud(serial, baud).map_err(|e| e.to_string())?;
    port.wake()?;
    let mut reader = Reader::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(75);
    let mut next = std::time::Instant::now();
    let method = if operation == "flash.wifi.join" {
        21
    } else {
        22
    };
    let result = loop {
        if std::time::Instant::now() >= deadline {
            return Err("no recovery Wi-Fi reply within 75 seconds".into());
        }
        if std::time::Instant::now() >= next {
            port.write_all(&encode_record(248, &[]))
                .map_err(|e| e.to_string())?;
            port.write_all(&encode_record(250, &request))
                .map_err(|e| e.to_string())?;
            next = std::time::Instant::now() + Duration::from_secs(1);
        }
        let bytes = port.read().map_err(|e| e.to_string())?;
        let mut answer = None;
        for event in reader.feed(&bytes) {
            if let Event::Record(250, data) = event {
                if let Ok(v) = crate::console::decode(&data) {
                    if v["1"] == 44 && v["2"] == method && v["3"].as_u64() == Some(id) {
                        if let Some(error) = v.get("7") {
                            return Err(format!("recovery Wi-Fi error {error}"));
                        }
                        answer = v.get("6").cloned();
                    }
                }
            }
        }
        let text = reader.take_text();
        if !text.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&text));
        }
        if let Some(result) = answer {
            break result;
        }
        if bytes.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let info = wifi_result(&result)?;
    println!("{info}");
    if tail.is_empty() {
        return Ok(());
    }
    let tcp = info["tcp"]
        .as_str()
        .ok_or("station has no IP; join a network before uploading")?;
    let mut upload = vec![tcp.to_owned()];
    upload.extend_from_slice(tail);
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let logs = std::thread::spawn(move || {
        let mut next = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if std::time::Instant::now() >= next {
                if port.write_all(&encode_record(248, &[])).is_err() {
                    break;
                }
                next = std::time::Instant::now() + Duration::from_secs(1);
            }
            match port.read() {
                Ok(bytes) => {
                    reader.feed(&bytes);
                    let text = reader.take_text();
                    if !text.is_empty() {
                        eprint!("{}", String::from_utf8_lossy(&text));
                    }
                    if bytes.is_empty() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                Err(_) => break,
            }
        }
    });
    // IP readiness can precede the runtime reopening its declarative TCP listener.
    let result = run_image(&upload, Duration::from_secs(30), true);
    done.store(true, Ordering::Relaxed);
    let _ = logs.join();
    result
}
fn wifi_request(operation: &str, args: &[String], id: u64) -> Result<Vec<u8>, String> {
    let method = match operation {
        "flash.wifi.join" => 21,
        "flash.wifi.info" => 22,
        _ => return Err("unknown recovery Wi-Fi method".into()),
    };
    let mut ssid = None;
    let mut psk = None;
    for arg in args {
        let (k, v) = arg
            .strip_prefix("--")
            .and_then(|a| a.split_once('='))
            .ok_or("expected --ssid=NAME --psk=PASSPHRASE")?;
        match k {
            "ssid" if ssid.is_none() && method == 21 => ssid = Some(v),
            "psk" if psk.is_none() && method == 21 => psk = Some(v),
            _ => return Err(format!("unexpected recovery Wi-Fi option {k}")),
        }
    }
    if method == 21 {
        let ssid = ssid.ok_or("--ssid is required")?;
        if ssid.is_empty() || ssid.len() > 32 || ssid.contains('\0') {
            return Err("SSID must be 1..32 bytes without NUL".into());
        }
        let psk = psk.unwrap_or("");
        if (!psk.is_empty() && !(8..=63).contains(&psk.len())) || psk.contains('\0') {
            return Err("PSK must be empty or 8..63 bytes without NUL".into());
        }
    }
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(4)
        .unwrap()
        .u8(1)
        .unwrap()
        .u8(44)
        .unwrap()
        .u8(2)
        .unwrap()
        .u8(method)
        .unwrap()
        .u8(3)
        .unwrap()
        .u64(id)
        .unwrap()
        .u8(5)
        .unwrap()
        .map(if method == 21 { 2 } else { 0 })
        .unwrap();
    if method == 21 {
        e.u8(1)
            .unwrap()
            .str(ssid.unwrap())
            .unwrap()
            .u8(2)
            .unwrap()
            .str(psk.unwrap_or(""))
            .unwrap();
    }
    Ok(e.into_writer())
}
fn wifi_result(result: &serde_json::Value) -> Result<serde_json::Value, String> {
    let ready = result["1"]
        .as_bool()
        .ok_or("invalid recovery Wi-Fi response")?;
    let ip = result["3"].as_str().ok_or("response missing IP")?;
    let bytes = ip
        .strip_prefix("hex:")
        .filter(|h| h.len() == 8)
        .ok_or("invalid recovery IP")?;
    let mut octets = [0; 4];
    for (i, octet) in octets.iter_mut().enumerate() {
        *octet =
            u8::from_str_radix(&bytes[i * 2..i * 2 + 2], 16).map_err(|_| "invalid recovery IP")?;
    }
    let ip = std::net::Ipv4Addr::from(octets);
    let mac = result["6"]
        .as_str()
        .and_then(|s| s.strip_prefix("hex:"))
        .filter(|s| s.len() == 12 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("invalid recovery MAC")?;
    let mac = (0..6)
        .map(|i| &mac[i * 2..i * 2 + 2])
        .collect::<Vec<_>>()
        .join(":");
    let port = result["4"]
        .as_u64()
        .filter(|p| *p == 5227)
        .ok_or("invalid recovery TCP port")?;
    Ok(
        serde_json::json!({"ready":ready,"ssid":result["2"],"ip":ip.to_string(),"port":port,
        "station":result["5"],"mac":mac,"channel":result["7"],
        "tcp":if ready && !ip.is_unspecified() {Some(format!("tcp://{ip}:{port}"))} else {None}}),
    )
}

#[cfg(test)]
mod wifi_tests {
    use super::*;
    #[test]
    fn credentials_are_text_and_validated_without_echoing_secrets() {
        let args = ["--ssid=00123".into(), "--psk=0123456789".into()];
        let v =
            crate::console::decode(&wifi_request("flash.wifi.join", &args, 77).unwrap()).unwrap();
        assert_eq!(v["5"]["1"], "00123");
        assert_eq!(v["5"]["2"], "0123456789");
        assert_eq!(v["3"], 77);
        assert!(wifi_request(
            "flash.wifi.join",
            &["--ssid=test".into(), "--psk=short".into()],
            1
        )
        .is_err());
        assert!(wifi_request("flash.wifi.info", &args, 1).is_err());
    }
    #[test]
    fn info_only_provides_upload_endpoint_when_station_has_an_address() {
        let v = serde_json::json!({"1":true,"2":"lab","3":"hex:c0a80102","4":5227,"6":"hex:10bda3ac5a20"});
        assert_eq!(wifi_result(&v).unwrap()["tcp"], "tcp://192.168.1.2:5227");
        let mut v = v;
        v["1"] = false.into();
        assert!(wifi_result(&v).unwrap()["tcp"].is_null());
    }
}

#[cfg(test)]
mod image_retry_tests {
    use super::*;
    #[test]
    fn lost_commit_reply_retransmits_the_identical_image_operation() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let data = vec![0x5a; 256];
        let expected = data.clone();
        let receiver = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    let mut d = minicbor::Decoder::new(&request);
                    if d.skip().is_ok() && d.position() == request.len() {
                        break;
                    }
                }
                let fields = crate::console::decode(&request).unwrap();
                assert_eq!(fields["5"]["8"], 7);
                let mut body = vec![0; expected.len()];
                stream.read_exact(&mut body).unwrap();
                assert_eq!(body, expected);
                request.extend_from_slice(&body);
                requests.push(request);
                if attempt == 1 {
                    let mut e = minicbor::Encoder::new(Vec::new());
                    e.map(3)
                        .unwrap()
                        .u8(1)
                        .unwrap()
                        .u8(5)
                        .unwrap()
                        .u8(2)
                        .unwrap()
                        .u16(256)
                        .unwrap()
                        .u8(6)
                        .unwrap()
                        .u32(0x190000)
                        .unwrap();
                    stream.write_all(&e.into_writer()).unwrap();
                }
                // First request commits its payload, then loses its reply.
            }
            assert_eq!(requests[0], requests[1]);
        });
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/cli-retry-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join(format!("image-{}.bin", std::process::id()));
        std::fs::write(&file, data).unwrap();
        let result = run(&[
            format!("tcp://{address}"),
            "flash.module.write".into(),
            "--name=flash".into(),
            "--version=5".into(),
            "--operation_id=7".into(),
            format!("--input={}", file.display()),
        ]);
        std::fs::remove_file(file).unwrap();
        result.unwrap();
        receiver.join().unwrap();
    }
}
