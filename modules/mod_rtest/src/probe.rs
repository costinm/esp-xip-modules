//! Canonical DMesh probe service over either native TCP or module streams.
use super::*;
use mesh_api::envelope::{self as tagged, Name, Record};
use quic_tools::{self as service, DEFAULT_PROBE_MAX_STREAM_PAYLOAD, ProbePayload};
const TICK: i32 = 90;
const MAX_HEADER: usize = service::PROBE_REQUEST_MAX;
struct Connection {
    handle: u32,
    quic: bool,
    header: Vec<u8>,
    request: Option<service::ProbeServiceRequest>,
    id: u64,
    upload: Option<ProbePayload>,
    download: Option<ProbePayload>,
    ended: bool,
    response: [u8; 96],
    response_len: usize,
    packet: u32,
    packet_len: usize,
    packet_final: bool,
    tcp_pending: bool,
    done: bool,
    last: u64,
    next_read: u64,
    rx: u32,
    rx_len: usize,
    rx_at: usize,
}
impl Connection {
    fn new(handle: u32, quic: bool, now: u64) -> Self {
        Self {
            handle,
            quic,
            header: Vec::new(),
            request: None,
            id: 0,
            upload: None,
            download: None,
            ended: false,
            response: [0; 96],
            response_len: 0,
            packet: 0,
            packet_len: 0,
            packet_final: false,
            tcp_pending: false,
            done: false,
            last: now,
            next_read: now,
            rx: 0,
            rx_len: 0,
            rx_at: 0,
        }
    }
    fn open(&mut self, record: Record<'_>, now: u64) -> Result<(), i32> {
        let (id, request) = service::decode_probe_handler_record(record).ok_or(ERR_ARG)?;
        // Same supported workload as the canonical responder: duplex is two
        // independent streams. Remote probe orchestration remains host policy.
        if request.direction == service::ProbeDirection::Both
            || request.bytes > service::MAX_BYTES
            || request.peer.is_some()
            || request.parallel_streams.is_some_and(|v| v != 1)
            || request.high_priority_bytes.is_some_and(|v| v != 0)
            || request.low_priority_bytes.is_some_and(|v| v != 0)
            || request.ack_frequency.is_some()
            || request.ack_delay_ms.is_some()
        {
            return Err(ERR_UNSUPPORTED);
        }
        let packet_size = service::ProbePlan::from_request(
            request.workload(),
            DEFAULT_PROBE_MAX_STREAM_PAYLOAD,
        )
        .packet_size;
        self.upload = if request.upload_bytes() == 0 {
            None
        } else {
            Some(ProbePayload::new(request.upload_bytes(), packet_size).ok_or(ERR_ARG)?)
        };
        self.download = if request.download_bytes() == 0 {
            None
        } else {
            Some(ProbePayload::new(request.download_bytes(), packet_size).ok_or(ERR_ARG)?)
        };
        self.id = id;
        self.request = Some(request);
        self.header = Vec::new();
        self.next_read = now.saturating_add(request.initial_consume_delay_ms.unwrap_or(0) as u64);
        let mut value = [0; 48];
        let mut w = Writer::new(&mut value);
        w.map(if request.upload_bytes() == 0 { 2 } else { 3 })
            .uint(1)
            .uint(request.download_bytes())
            .uint(2)
            .uint(packet_size as u64);
        if request.upload_bytes() != 0 {
            w.uint(3).uint(request.upload_bytes());
        }
        self.response_len = tagged::encode_numeric_response(
            service::PROBE_COMPONENT,
            service::PROBE_HANDLER,
            id,
            w.finish().ok_or(ERR_SIZE)?,
            &mut self.response,
        )
        .ok_or(ERR_SIZE)?;
        Ok(())
    }
    fn feed(&mut self, bytes: &[u8], now: u64) -> Result<usize, i32> {
        if self.ended {
            return Err(ERR_ARG);
        }
        if now < self.next_read {
            return Ok(0);
        }
        let mut at = 0;
        while self.request.is_none() && at < bytes.len() {
            if self.header.is_empty() {
                match cbor::decode_envelope_prefix(&bytes[at..], MAX_HEADER) {
                    cbor::DevicePrefix::Complete {
                        envelope,
                        body_offset,
                    } => {
                        at += body_offset;
                        let record = Record {
                            component: Some(Name::Tag(envelope.component as u64)),
                            method: Some(Name::Tag(envelope.method as u64)),
                            id: envelope.id,
                            fields: Some(envelope.fields),
                            ..Record::default()
                        };
                        self.open(record, now)?;
                        break;
                    }
                    cbor::DevicePrefix::Incomplete => {}
                    cbor::DevicePrefix::Invalid => {
                        return Err(if bytes[at..].len() > MAX_HEADER {
                            ERR_SIZE
                        } else {
                            ERR_ARG
                        });
                    }
                }
            }
            if self.header.len() >= MAX_HEADER {
                return Err(ERR_SIZE);
            }
            self.header.try_reserve(1).map_err(|_| ERR_NOMEM)?;
            self.header.push(bytes[at]);
            at += 1;
            let header = core::mem::take(&mut self.header);
            match cbor::decode_envelope_prefix(&header, MAX_HEADER) {
                cbor::DevicePrefix::Complete { envelope, .. } => {
                    let record = Record {
                        component: Some(Name::Tag(envelope.component as u64)),
                        method: Some(Name::Tag(envelope.method as u64)),
                        id: envelope.id,
                        fields: Some(envelope.fields),
                        ..Record::default()
                    };
                    self.open(record, now)?;
                }
                cbor::DevicePrefix::Incomplete => {
                    self.header = header;
                }
                cbor::DevicePrefix::Invalid => return Err(ERR_ARG),
            }
        }
        if self.request.is_some() && at < bytes.len() {
            if now < self.next_read {
                return Ok(at);
            }
            let request = self.request.unwrap();
            self.upload
                .as_mut()
                .ok_or(ERR_ARG)?
                .consume(&bytes[at..], request.verify)
                .map_err(|_| ERR_ARG)?;
            self.next_read = now.saturating_add(request.consume_delay_ms.unwrap_or(0) as u64);
            at = bytes.len();
        }
        self.last = now;
        Ok(at)
    }
    fn end(&mut self) -> Result<(), i32> {
        if self.request.is_none() || self.upload.as_ref().is_some_and(|p| !p.is_complete()) {
            return Err(ERR_ARG);
        }
        self.ended = true;
        Ok(())
    }
    fn dispose(&mut self, hw: Hw) {
        if self.rx != 0 {
            hw.pkt_free(self.rx);
            self.rx = 0;
        }
        if self.packet != 0 {
            hw.pkt_free(self.packet);
            self.packet = 0;
        }
        if self.quic {
            if !self.packet_final {
                if let Ok(t) = stream(hw) {
                    if let Some(close) = t.close {
                        unsafe {
                            let _ = close(hw.ctx(), self.handle);
                        }
                    }
                }
            }
        } else {
            hw.socket_close(self.handle);
        }
    }
    fn progress(&mut self, hw: Hw) -> Result<(), i32> {
        if hw.now_ms().saturating_sub(self.last) > 30_000 {
            return Err(ERR_AGAIN);
        }
        for _ in 0..8 {
            if !self.ended || self.done || self.tcp_pending {
                return Ok(());
            }
            if self.packet == 0 {
                self.packet = match hw.pkt_alloc() {
                    Ok(p) => p,
                    Err(ERR_NOMEM | ERR_BUSY | ERR_AGAIN) => return Ok(()),
                    Err(e) => return Err(e),
                };
                let (p, cap) = hw.pkt_buf(self.packet)?;
                let max_chunk = if self.quic { cap.min(1380) } else { cap };
                let out = unsafe { core::slice::from_raw_parts_mut(p, max_chunk) };
                let mut n = self.response_len;
                out.get_mut(..n)
                    .ok_or(ERR_SIZE)?
                    .copy_from_slice(&self.response[..n]);
                self.response_len = 0;
                if let Some(payload) = self.download.as_mut() {
                    n += payload.fill(&mut out[n..]);
                }
                self.packet_len = n;
                self.packet_final = self.download.as_ref().is_none_or(ProbePayload::is_complete);
            }
            let rc = if self.quic {
                let t = stream(hw)?;
                unsafe {
                    t.write_pkt.ok_or(ERR_UNSUPPORTED)?(
                        hw.ctx(),
                        self.handle,
                        self.packet,
                        self.packet_len as u32,
                        self.packet_final as u32,
                    )
                }
            } else {
                hw.socket_write_pkt(
                    self.handle,
                    self.packet,
                    PKT_HEADROOM,
                    self.packet_len as u32,
                )
            };
            if rc == ERR_BUSY || rc == ERR_AGAIN || rc == ERR_NOMEM {
                return Ok(());
            }
            if rc < 0 {
                return Err(rc);
            }
            self.packet = 0;
            self.last = hw.now_ms();
            if self.quic {
                self.done = self.packet_final;
            } else {
                self.tcp_pending = true;
            }
        }
        Ok(())
    }
}
fn stream(hw: Hw) -> Result<&'static xip_abi::Stream, i32> {
    hw.stream().ok_or(ERR_UNSUPPORTED)
}

#[derive(Default)]
pub struct Driver {
    connections: Vec<Connection>,
    alarm: i32,
}
impl Driver {
    pub fn start(&mut self, hw: Hw) -> i32 {
        if let Ok(t) = stream(hw) {
            if let Some(f) = t.listen {
                let rc = unsafe { f(hw.ctx(), 8, 2) };
                if rc < 0 {
                    return rc;
                }
            }
        }
        self.alarm = hw.alarm_set(5, 0, 5, ALARM_EXACT, C_TEST, TICK);
        if self.alarm < 0 {
            self.alarm
        } else {
            0
        }
    }
    pub fn stop(&mut self, hw: Hw) {
        for c in &mut self.connections {
            c.dispose(hw);
        }
        self.connections.clear();
        if self.alarm >= 0 {
            hw.alarm_cancel(self.alarm);
        }
    }
    pub fn handle(&mut self, hw: Hw, call: &CallView) -> i32 {
        if call.component == C_TEST && call.method == 2 && call.wants_response() {
            let mut out = [0; 256];
            let mut w = Writer::new(&mut out);
            w.map(1).uint(1).array(self.connections.len());
            for c in &self.connections {
                w.map(8)
                    .uint(1)
                    .uint(c.handle as u64)
                    .uint(2)
                    .boolean(c.quic)
                    .uint(3)
                    .boolean(c.request.is_some())
                    .uint(4)
                    .boolean(c.ended)
                    .uint(5)
                    .boolean(c.tcp_pending)
                    .uint(6)
                    .uint(c.packet as u64)
                    .uint(7)
                    .uint(c.packet_len as u64)
                    .uint(8)
                    .boolean(c.done);
            }
            if let Some(bytes) = w.finish() {
                hw.reply(call, bytes);
            }
            return 0;
        }
        let quic = call.flags & xip_abi::CALL_STREAM != 0;
        if quic && (call.component != 8 || call.method != 2) {
            return ERR_UNSUPPORTED;
        }
        let opening = if quic {
            call.value == xip_abi::STREAM_OPEN
        } else {
            call.method == 0 && call.value == xip_abi::SOCKET_EVENT_OPEN as i32
        };
        if opening {
            if self.connections.len() >= 2 {
                if !quic {
                    hw.socket_close(call.id as u32);
                }
                return if quic { ERR_BUSY } else { 0 };
            }
            if self.connections.try_reserve(1).is_err() {
                return if quic { ERR_NOMEM } else { 0 };
            }
            let mut c = Connection::new(call.id as u32, quic, hw.now_ms());
            if quic {
                let record = Record {
                    component: Some(Name::Tag(8)),
                    method: Some(Name::Tag(2)),
                    id: call.request_id,
                    fields: Some(call.data),
                    ..Record::default()
                };
                if let Err(e) = c.open(record, hw.now_ms()) {
                    return e;
                }
            }
            self.connections.push(c);
        }
        if let Some(c) = self
            .connections
            .iter_mut()
            .find(|c| c.quic == quic && c.handle == call.id as u32)
        {
            if quic {
                match call.value {
                    xip_abi::STREAM_DATA => {
                        return c
                            .feed(call.data, hw.now_ms())
                            .map_or_else(|e| e, |n| n as i32)
                    }
                    xip_abi::STREAM_FIN => {
                        if let Err(e) = c.end() {
                            return e;
                        }
                    }
                    xip_abi::STREAM_CLOSED => c.done = true,
                    _ => {}
                }
            } else if call.method == 0 {
                match call.value as u32 {
                    xip_abi::SOCKET_EVENT_WRITE_DONE => {
                        c.tcp_pending = false;
                        if c.packet_final {
                            c.done = true;
                        }
                    }
                    xip_abi::SOCKET_EVENT_CLOSED => c.done = true,
                    _ => {}
                }
            }
        }
        for c in &mut self.connections {
            if !c.quic && !c.ended && !c.done && hw.now_ms() >= c.next_read {
                for _ in 0..4 {
                    if c.rx == 0 {
                        match hw.pkt_alloc() {
                            Ok(p) => c.rx = p,
                            Err(_) => break,
                        }
                    }
                    let Ok((p, cap)) = hw.pkt_buf(c.rx) else {
                        c.done = true;
                        break;
                    };
                    if c.rx_at == c.rx_len {
                        let n = hw.socket_read(c.handle, unsafe {
                            core::slice::from_raw_parts_mut(p, cap.min(512))
                        });
                        if n == ERR_AGAIN {
                            break;
                        }
                        if n < 0 {
                            c.done = true;
                            break;
                        }
                        if n == 0 {
                            if c.end().is_err() {
                                c.done = true;
                            }
                            break;
                        }
                        c.rx_at = 0;
                        c.rx_len = n as usize;
                    }
                    let bytes =
                        unsafe { core::slice::from_raw_parts(p.add(c.rx_at), c.rx_len - c.rx_at) };
                    match c.feed(bytes, hw.now_ms()) {
                        Ok(n) => c.rx_at += n,
                        Err(_) => {
                            c.done = true;
                            break;
                        }
                    }
                    if c.rx_at != c.rx_len || hw.now_ms() < c.next_read {
                        break;
                    }
                }
            }
            if c.progress(hw).is_err() {
                c.done = true;
            }
        }
        self.connections.retain_mut(|c| {
            if c.done {
                c.dispose(hw);
                false
            } else {
                true
            }
        });
        0
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    fn header(request: service::ProbeServiceRequest) -> Vec<u8> {
        let mut b = [0; MAX_HEADER];
        let n = service::encode_probe_handler_request(request, 17, &mut b).unwrap();
        b[..n].to_vec()
    }
    // Compare the embedded adapter with the pinned portable responder itself,
    // not a second expected response assembled by this test.
    #[test]
    fn responder_matches_portable_probe_handler_headers_and_payloads() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            for direction in [service::ProbeDirection::Upload, service::ProbeDirection::Download] {
                for packet_size in [8, 511, 1036] {
                    for verify in [false, true] {
                        let mut request = service::ProbeServiceRequest::new(8193, packet_size);
                        request.direction = direction;
                        request.verify = verify;
                        let h = header(request);
                        let record = tagged::decode(&h).unwrap();
                        let mut embedded = Connection::new(1, true, 0);
                        embedded.open(record, 0).unwrap();

                        let (mut client, mut server) = tokio::io::duplex(32 * 1024);
                        let responder = tokio::spawn(async move {
                            service::serve_probe_stream(&mut server).await.unwrap();
                        });

                        client.write_all(&h).await.unwrap();
                        if request.upload_bytes() > 0 {
                            let mut source =
                                ProbePayload::new(request.upload_bytes(), packet_size as usize)
                                    .unwrap();
                            let mut bytes = [0; 389];
                            while !source.is_complete() {
                                let n = source.fill(&mut bytes);
                                client.write_all(&bytes[..n]).await.unwrap();
                                assert_eq!(embedded.feed(&bytes[..n], 0).unwrap(), n);
                            }
                        }
                        client.shutdown().await.unwrap();
                        embedded.end().unwrap();

                        let mut ref_wire = Vec::new();
                        client.read_to_end(&mut ref_wire).await.unwrap();
                        responder.await.unwrap();

                        let mut dec = mesh_api::cbor::Decoder::new(&ref_wire);
                        dec.skip().unwrap();
                        let ref_hdr_len = dec.position();
                        let ref_header = &ref_wire[..ref_hdr_len];
                        let ref_body = &ref_wire[ref_hdr_len..];

                        // CBOR integer widths may differ between encoders; compare
                        // decoded envelope/results and require identical body bytes.
                        let actual = &embedded.response[..embedded.response_len];
                        for tag in [1, 2, 3] {
                            assert_eq!(cbor::get_int(actual, tag), cbor::get_int(ref_header, tag));
                        }
                        let a = tagged::decode(actual).unwrap();
                        let b = tagged::decode(ref_header).unwrap();
                        for tag in [1, 2, 3] {
                            assert_eq!(
                                cbor::get_int(a.result.unwrap(), tag),
                                cbor::get_int(b.result.unwrap(), tag)
                            );
                        }
                        if request.download_bytes() > 0 {
                            let mut actual_body = Vec::new();
                            let mut chunk = [0; 1493];
                            while !embedded.download.as_ref().unwrap().is_complete() {
                                let count = embedded.download.as_mut().unwrap().fill(&mut chunk);
                                actual_body.extend_from_slice(&chunk[..count]);
                            }
                            assert_eq!(ref_body, actual_body.as_slice());
                        } else {
                            assert!(ref_body.is_empty());
                            assert!(embedded.download.is_none());
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn canonical_upload_is_fragmentation_and_transport_independent() {
        let mut request = service::ProbeServiceRequest::new(16384, 257);
        request.direction = service::ProbeDirection::Upload;
        request.verify = true;
        let h = header(request);
        let mut tcp = Connection::new(1, false, 0);
        for b in &h {
            assert_eq!(tcp.feed(core::slice::from_ref(b), 0), Ok(1));
        }
        let mut quic = Connection::new(2, true, 0);
        quic.open(tagged::decode(&h).unwrap(), 0).unwrap();
        let mut source = ProbePayload::new(request.bytes, 257).unwrap();
        let mut bytes = [0; 389];
        while !source.is_complete() {
            let n = source.fill(&mut bytes);
            assert_eq!(tcp.feed(&bytes[..n], 1), Ok(n));
            assert_eq!(quic.feed(&bytes[..n], 1), Ok(n));
        }
        tcp.end().unwrap();
        quic.end().unwrap();
        assert_eq!(tcp.response, quic.response);
        assert_eq!(tcp.response_len, quic.response_len);
        let response = tagged::decode(&tcp.response[..tcp.response_len]).unwrap();
        assert_eq!(response.id, Some(17));
        assert_eq!(
            cbor::get_int(response.result.unwrap(), 3).unwrap(),
            Some(16384)
        );
        assert!(quic.feed(b"late", 2).is_err());
    }
    #[test]
    fn short_upload_corruption_and_consumer_delays_are_real_backpressure() {
        let mut request = service::ProbeServiceRequest::new(100, 32);
        request.direction = service::ProbeDirection::Upload;
        request.verify = true;
        request.initial_consume_delay_ms = Some(5);
        request.consume_delay_ms = Some(3);
        let h = header(request);
        let mut c = Connection::new(1, true, 0);
        c.open(tagged::decode(&h).unwrap(), 0).unwrap();
        let mut payload = ProbePayload::new(100, 32).unwrap();
        let mut bytes = [0; 50];
        let n = payload.fill(&mut bytes);
        assert_eq!(c.feed(&bytes[..n], 4), Ok(0));
        assert_eq!(c.feed(&bytes[..n], 5), Ok(n));
        assert!(c.end().is_err());
        assert_eq!(c.feed(&bytes[..n], 6), Ok(0));
        bytes[0] ^= 1;
        assert!(c.feed(&bytes[..n], 8).is_err());
    }
    #[test]
    fn download_pattern_matches_canonical_client_across_packet_boundaries() {
        let request = service::ProbeServiceRequest::new(65536, 511);
        let h = header(request);
        let mut c = Connection::new(1, true, 0);
        c.open(tagged::decode(&h).unwrap(), 0).unwrap();
        c.end().unwrap();
        let mut check = ProbePayload::new(65536, 511).unwrap();
        let mut bytes = [0; 1493];
        let source = c.download.as_mut().unwrap();
        while !source.is_complete() {
            let n = source.fill(&mut bytes);
            check.check(&bytes[..n]).unwrap();
        }
        assert!(check.is_complete());
    }
}
