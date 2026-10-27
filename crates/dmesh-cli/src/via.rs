//! Explicit circuit transport. This never creates forwarding rules.
use crate::node_client::{ClientPath, Node, Pool};
use crate::xip_wire::{encode_record, Event, Reader, XipPort};
use quic_lite::{
    BearerContext, BearerInfo, BearerName, EgressSubmission, OwnedPacket, PacketBearer,
    PacketEgress, PacketMeta, PacketPool, PacketSendOutcome, PacketSubmitError, PacketWriter,
    PeerL2Address,
};
use std::{
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
type Buffer = <Pool as PacketPool>::Buffer;
#[derive(Clone, Debug)]
pub(crate) struct Via {
    pub target: String,
    pub dcid: Vec<u8>,
}
fn hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("hex:").unwrap_or(s);
    if s.is_empty() || s.len() > 40 || s.len() % 2 != 0 || !s.is_ascii() {
        return Err("DCID must be 1..20 hex bytes".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "invalid DCID".into()))
        .collect()
}
fn cid_len(b: u8) -> usize {
    if b & 128 == 0 {
        1
    } else {
        match b >> 5 {
            4 => 2,
            5 => 4,
            6 => 8,
            _ => 0,
        }
    }
}
impl Via {
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.contains('#') {
            return Err("--via uses FIRST_HOP:DCID; no return DCID is needed".into());
        }
        let (target, cid) = s
            .split_once(":hex:")
            .or_else(|| s.rsplit_once(':'))
            .ok_or("--via requires FIRST_HOP:DCID")?;
        let dcid = hex(cid)?;
        if target.is_empty() || cid_len(dcid[0]) != dcid.len() {
            return Err("circuit DCID must use the native short-header CID class".into());
        }
        Ok(Self {
            target: target.into(),
            dcid,
        })
    }
    fn prefix(&self) -> Vec<u8> {
        let mut h = vec![0x40];
        h.extend(&self.dcid);
        h.push(0);
        h
    }
    fn unwrap<'a>(&self, p: &'a [u8]) -> Option<&'a [u8]> {
        if p.len() < 3 || p[0] != 0x40 {
            return None;
        }
        let n = cid_len(p[1]);
        (n != 0 && p.len() > n + 2 && p[n + 1] == 0).then(|| &p[n + 2..])
    }
    pub fn open(&self, baud: Option<u32>) -> Result<Io, String> {
        let peer = if self.target.starts_with("udp://") {
            Some(crate::client::parse_udp_peer(&self.target)?)
        } else {
            crate::client::parse_udp_peer(&self.target).ok()
        };
        if let Some(peer) = peer {
            let s = std::net::UdpSocket::bind(crate::client::udp_bind_for_peer(peer))
                .map_err(|e| e.to_string())?;
            s.connect(peer).map_err(|e| e.to_string())?;
            s.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(Io::Udp(s))
        } else {
            let (path, baud) = if self.target.starts_with('/') {
                (self.target.clone(), baud)
            } else {
                let p = crate::device::load_device(&self.target)?;
                (
                    p.serial_path()?
                        .ok_or("via device has no serial path")?
                        .display()
                        .to_string(),
                    baud.or(p.uart_baud),
                )
            };
            let mut port = XipPort::open_baud(path, baud).map_err(|e| e.to_string())?;
            port.wake()?;
            Ok(Io::Uart(port, Reader::default()))
        }
    }
    pub fn attach(&self, node: &mut Node, baud: Option<u32>) -> Result<ClientPath, String> {
        let io = Arc::new(Mutex::new(self.open(baud)?));
        let bearer = node
            .add_bearer(Circuit {
                io,
                via: self.clone(),
                task: None,
                blocked: Arc::new(AtomicBool::new(false)),
            })
            .map_err(|e| format!("circuit bearer: {e:?}"))?;
        Ok(ClientPath {
            bearer,
            peer_l2_address: PeerL2Address::new(1).unwrap(),
        })
    }
    pub fn exchange(&self, baud: Option<u32>, request: &[u8]) -> Result<Vec<u8>, String> {
        let mut io = self.open(baud)?;
        io.send(&self.prefix(), request)
            .map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while std::time::Instant::now() < deadline {
            if let Some(p) = io.receive().map_err(|e| e.to_string())? {
                if let Some(inner) = self.unwrap(&p) {
                    return Ok(inner.to_vec());
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err("explicit circuit response timed out".into())
    }
}
pub(crate) enum Io {
    Udp(std::net::UdpSocket),
    Uart(XipPort, Reader),
}
impl Io {
    fn send(&mut self, header: &[u8], body: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Udp(socket) => {
                let mut vectors = [
                    libc::iovec {
                        iov_base: header.as_ptr() as *mut _,
                        iov_len: header.len(),
                    },
                    libc::iovec {
                        iov_base: body.as_ptr() as *mut _,
                        iov_len: body.len(),
                    },
                ];
                let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                msg.msg_iov = vectors.as_mut_ptr();
                msg.msg_iovlen = 2;
                let n = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, libc::MSG_DONTWAIT) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else if n as usize == header.len() + body.len() {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "partial UDP send",
                    ))
                }
            }
            Self::Uart(port, _) => {
                let mut packet = header.to_vec();
                packet.extend(body);
                port.write_all(&encode_record(247, &packet))
            }
        }
    }
    fn receive_packet(
        &mut self,
        via: &Via,
        context: &BearerContext<Pool>,
    ) -> std::io::Result<bool> {
        match self {
            Self::Udp(socket) => {
                let Some(mut writer) = context.pool().acquire_writer(144, 0) else {
                    return Ok(false);
                };
                let n = match socket.recv(writer.payload_mut()) {
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) => return Err(e),
                };
                let prefix = if n > 1 {
                    cid_len(writer.payload_mut()[1]) + 2
                } else {
                    0
                };
                if via.unwrap(&writer.payload_mut()[..n]).is_some() {
                    if let Some(packet) = writer.commit(n) {
                        let (buffer, range) = packet.into_parts();
                        let packet =
                            OwnedPacket::new(buffer, range.start + prefix..range.end).unwrap();
                        context.enqueue_packet(
                            PacketMeta {
                                bearer: context.bearer(),
                                peer_l2_address: PeerL2Address::new(1).unwrap(),
                                received_at_us: 0,
                            },
                            packet,
                        );
                    }
                }
                Ok(true)
            }
            Self::Uart(port, r) => {
                let bytes = port.read()?;
                let progress = !bytes.is_empty();
                for event in r.feed(&bytes) {
                    if let Event::Record(247 | 244, bytes) = event {
                        if let Some(inner) = via.unwrap(&bytes) {
                            let Some(mut writer) = context.pool().acquire_writer(144, 0) else {
                                continue;
                            };
                            if inner.len() > writer.payload_mut().len() {
                                continue;
                            }
                            writer.payload_mut()[..inner.len()].copy_from_slice(inner);
                            if let Some(packet) = writer.commit(inner.len()) {
                                context.enqueue_packet(
                                    PacketMeta {
                                        bearer: context.bearer(),
                                        peer_l2_address: PeerL2Address::new(1).unwrap(),
                                        received_at_us: 0,
                                    },
                                    packet,
                                );
                            }
                        }
                    }
                }
                r.take_text();
                Ok(progress)
            }
        }
    }
    fn receive(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        match self {
            Self::Udp(s) => {
                let mut b = vec![0; 1500];
                match s.recv(&mut b) {
                    Ok(n) => {
                        b.truncate(n);
                        Ok(Some(b))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Self::Uart(port, r) => {
                for event in r.feed(&port.read()?) {
                    if let Event::Record(247 | 244, b) = event {
                        return Ok(Some(b));
                    }
                }
                r.take_text();
                Ok(None)
            }
        }
    }
}
struct Circuit {
    io: Arc<Mutex<Io>>,
    via: Via,
    task: Option<tokio::task::JoinHandle<()>>,
    blocked: Arc<AtomicBool>,
}
impl Drop for Circuit {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}
impl PacketEgress<Buffer> for Circuit {
    fn submit(
        &mut self,
        _: PeerL2Address,
        submission: EgressSubmission<Buffer>,
    ) -> Result<(), PacketSubmitError<Buffer>> {
        // Do not return a possibly partially written UART frame for another path.
        let (udp, result) = {
            let mut io = self.io.lock().unwrap();
            (
                matches!(&*io, Io::Udp(_)),
                io.send(&self.via.prefix(), submission.packet().bytes()),
            )
        };
        if udp
            && result
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
        {
            self.blocked.store(true, Ordering::Release);
            return Err(PacketSubmitError::WouldBlock(submission));
        }
        submission.complete(
            if result.is_ok() {
                PacketSendOutcome::Sent
            } else {
                PacketSendOutcome::Failed
            },
            0,
        );
        Ok(())
    }
}
impl PacketBearer<Pool> for Circuit {
    type AttachError = ();
    fn info(&self) -> BearerInfo {
        BearerInfo {
            name: BearerName::new("circuit").unwrap(),
            max_packet_size: 1100,
            prefix_required: 0,
            suffix_required: 0,
            requires_packet_encryption: false,
            secure_link: false,
            nominal_bitrate_bps: 0,
            local_mac: None,
        }
    }
    fn attach(&mut self, context: BearerContext<Pool>) -> Result<(), ()> {
        let io = self.io.clone();
        let via = self.via.clone();
        let blocked = self.blocked.clone();
        self.task = Some(tokio::spawn(async move {
            loop {
                if blocked.swap(false, Ordering::AcqRel) {
                    context.send_ready();
                }
                let received = io.lock().unwrap().receive_packet(&via, &context);
                match received {
                    Err(_) => break,
                    Ok(false) => tokio::time::sleep(Duration::from_millis(1)).await,
                    Ok(true) => tokio::task::yield_now().await,
                }
            }
        }));
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn via_is_a_first_hop_and_one_dcid() {
        for bad in ["R1#02/01", "R1", "R1:0102", "R1:e0"] {
            assert!(Via::parse(bad).is_err(), "{bad}");
        }
        for spec in [
            "R1:02",
            "192.0.2.1:02",
            "[::1]:02",
            "/dev/serial/by-id/chip:02",
            "udp://127.0.0.1:3339:02",
            "udp://[::1]:3339:hex:02",
        ] {
            let v = Via::parse(spec).unwrap();
            assert_eq!(v.prefix(), [0x40, 2, 0]);
            assert_eq!(v.unwrap(&[0x40, 1, 0, 0xc0, 9]), Some(&[0xc0, 9][..]));
            assert_eq!(v.unwrap(&[0x40, 7, 0, 0xc0, 9]), Some(&[0xc0, 9][..]));
            assert!(v.unwrap(&[0x40, 1, 0]).is_none());
        }
    }
}
