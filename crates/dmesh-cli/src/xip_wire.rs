//! The runtime's serial record wire (`components/xip_runtime/src/xip_ppp.c`): a record is
//! `0x7e port payload 0x7e` with `0x7e`/`0x7d` escaped as `0x7d` (byte ^ `0x20`), the record's
//! first byte its port (`xip.h` `XIP_PORT_*`). Bytes between records are not the runtime's
//! workload and are dropped, but the device writes its ESP_LOG lines raw, so a session sees
//! both: log text and records. dmesh-cli is the only thing in this project that touches a
//! serial port and the only host-side implementation of this framing (AGENTS.md).

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;


/// The console port of the runtime's ports (`xip.h` `XIP_PORT_CONSOLE`): raw frames for a module that binds it; requests no longer go there.
pub const PORT_CONSOLE: u32 = 1;
/// The QUIC port (`xip.h` `XIP_PORT_QUIC`, 0xf7): the PPP record of a QUIC link. Every request to a device goes here, as a 0x444d0002 long packet.
pub const PORT_QUIC: u32 = 247;
/// The version of the CBOR datagram: after the SCID comes the envelope `{1: component, 2: method, 3: id, 5: fields}`.
pub const VERSION_CBOR: u32 = 0x444d_0002;

/// A QUIC long packet of `version` (first byte 0xc0, the version, DCID, SCID) around `payload`. The CIDs may be empty or up to 20 bytes
/// (`xip_quic.c` classifies them by length only).
pub fn long_packet(version: u32, dcid: &[u8], scid: &[u8], payload: &[u8]) -> Vec<u8> {
    assert!(dcid.len() <= 20 && scid.len() <= 20, "a connection ID is at most 20 bytes");
    let mut out = vec![0xc0];
    out.extend_from_slice(&version.to_be_bytes());
    out.push(dcid.len() as u8);
    out.extend_from_slice(dcid);
    out.push(scid.len() as u8);
    out.extend_from_slice(scid);
    out.extend_from_slice(payload);
    out
}

/// The request as a datagram for the device: a 0x444d0002 long packet (DCID 01, SCID 02) around the envelope.
pub fn wrap_request(envelope: &[u8]) -> Vec<u8> {
    long_packet(VERSION_CBOR, &[1], &[2], envelope)
}

/// The envelope inside an answer packet of the device (a 0x444d0002 long packet, our CIDs swapped), or None for anything else.
pub fn unwrap_answer(packet: &[u8]) -> Option<&[u8]> {
    let (&first, rest) = packet.split_first()?;
    if first & 0xc0 != 0xc0 || rest.len() < 6 || u32::from_be_bytes(rest[..4].try_into().ok()?) != VERSION_CBOR {
        return None;
    }
    let dcid = *rest.get(4)? as usize;
    let scid = *rest.get(5 + dcid)? as usize;
    if dcid > 20 || scid > 20 {
        return None;
    }
    rest.get(6 + dcid + scid..)
}
const FLAG: u8 = 0x7e;
const ESC: u8 = 0x7d;
const XOR: u8 = 0x20;

/// One record for `port`: `0x7e port payload 0x7e`, escaped.
pub fn encode_record(port: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(FLAG);
    out.push(port as u8);
    for &b in payload {
        if b == FLAG || b == ESC {
            out.push(ESC);
            out.push(b ^ XOR);
        } else {
            out.push(b);
        }
    }
    out.push(FLAG);
    out
}

/// One thing the wire carried: the device's raw text (its log lane) or a record of its port.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Text(Vec<u8>),
    Record(u32, Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside a record: text.
    Wait,
    /// After an opening flag, before the port byte.
    Start,
    /// In the bytes of a record.
    Body,
}

/// Cut a byte stream into text and records, the device's framing. One instance per session.
pub struct Reader {
    state: State,
    escaped: bool,
    port: u32,
    record: Vec<u8>,
    text: Vec<u8>,
}

impl Default for Reader {
    fn default() -> Self {
        Reader { state: State::Wait, escaped: false, port: 0, record: Vec::new(), text: Vec::new() }
    }
}

impl Reader {
    /// Feed one chunk; complete records come back as events (a record is complete at its
    /// closing flag). Text between records is retained and taken with [`take_text`](Self::take_text).
    /// The flag is matched on the raw byte, as on the wire: an escaped flag is two bytes and
    /// only resolves to a flag inside a record, never in the state machine.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        for &raw in bytes {
            if raw == FLAG {
                match self.state {
                    State::Wait => {
                        self.state = State::Start;
                    }
                    State::Start => {} // a doubled opening flag
                    State::Body => {
                        events.push(Event::Record(self.port, std::mem::take(&mut self.record)));
                        self.state = State::Wait;
                    }
                }
                self.escaped = false;
                continue;
            }
            let b = if self.escaped {
                self.escaped = false;
                raw ^ XOR
            } else if raw == ESC {
                self.escaped = true;
                continue;
            } else {
                raw
            };
            match self.state {
                State::Wait => self.text.push(b),
                State::Start => {
                    self.port = u32::from(b);
                    self.state = State::Body;
                }
                State::Body => self.record.push(b),
            }
        }
        events
    }

    /// The text accumulated since the last call.
    pub fn take_text(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.text)
    }
}

/// One exclusive session on the record wire. The open is the discipline of the `uart-codec`
/// bearer (one owner per port, raw terminal, no translation) and never touches the modem
/// lines: on the C6's USB-Serial/JTAG a DTR/RTS transition resets the chip.
pub struct XipPort {
    path: String,
    file: std::fs::File,
}

impl XipPort {
    pub fn open(path: impl Into<String>) -> std::io::Result<Self> {
        let path = path.into();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let fd = file.as_raw_fd();
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::ioctl(fd, libc::TIOCEXCL) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Self::set_raw(fd);
        Ok(XipPort { path, file })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn set_raw(fd: i32) {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut tio) } != 0 {
            return; // a port without termios keeps its line discipline
        }
        unsafe { libc::cfmakeraw(&mut tio) };
        tio.c_cflag |= libc::CREAD | libc::CLOCAL;
        // the runtime's fixed rate; irrelevant on USB-Serial/JTAG
        unsafe { libc::cfsetispeed(&mut tio, libc::B115200) };
        unsafe { libc::cfsetospeed(&mut tio, libc::B115200) };
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) };
    }

    /// Read everything currently available.
    pub fn read(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => return Ok(out), // EOF: the port went away
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(out),
                Err(e) => return Err(e),
            }
        }
    }

    pub fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.file.write_all(data)
    }

    /// Drop what accumulated before the first operation (a boot still streaming).
    pub fn flush_input(&mut self) -> std::io::Result<()> {
        let fd = self.file.as_raw_fd();
        let rc = unsafe { libc::ioctl(fd, libc::TCFLSH, libc::TCIFLUSH) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> (Vec<Event>, Vec<u8>) {
        let mut r = Reader::default();
        let events = r.feed(bytes);
        (events, r.take_text())
    }

    #[test]
    fn requests_are_long_packets_and_answers_are_unwrapped() {
        let envelope = [0xa2, 1, 20, 2, 5];
        let packet = wrap_request(&envelope);
        assert_eq!(&packet[..9], &[0xc0, 0x44, 0x4d, 0, 2, 1, 1, 1, 2]);
        assert_eq!(unwrap_answer(&packet), Some(&envelope[..]));
        // the device answers with our CIDs swapped, empty or long ones included
        for (d, s) in [(&[][..], &[][..]), (&[2][..], &[1][..]), (&[7; 20][..], &[9; 20][..])] {
            assert_eq!(unwrap_answer(&long_packet(VERSION_CBOR, d, s, &envelope)), Some(&envelope[..]));
        }
        assert_eq!(unwrap_answer(&long_packet(0x444d_0001, &[1], &[2], &envelope)), None, "another version is not ours");
        assert_eq!(unwrap_answer(&envelope), None);
        assert_eq!(unwrap_answer(&[0xc0, 0x44, 0x4d, 0, 2, 30, 0]), None, "a CID over 20 bytes");
        assert_eq!(unwrap_answer(&packet[..7]), None, "truncated");
    }

    #[test]
    fn a_record_carries_its_port_and_escapes_the_flag() {
        let wire = encode_record(3, &[1, FLAG, ESC, 9]);
        assert_eq!(wire[0], FLAG);
        assert_eq!(*wire.last().unwrap(), FLAG);
        assert!(!wire[1..wire.len() - 1].contains(&FLAG));
        let (events, text) = decode(&wire);
        assert_eq!(text, Vec::<u8>::new());
        assert_eq!(events, vec![Event::Record(3, vec![1, FLAG, ESC, 9])]);
    }

    #[test]
    fn records_in_a_row_and_in_pieces() {
        let mut wire = encode_record(2, b"one");
        wire.extend(encode_record(3, b"two"));
        let mut events = Vec::new();
        let mut r = Reader::default();
        for piece in wire.chunks(1) {
            events.extend(r.feed(piece));
        }
        assert_eq!(
            events,
            vec![Event::Record(2, Vec::from(b"one")), Event::Record(3, Vec::from(b"two"))]
        );
    }

    #[test]
    fn text_between_records_is_kept_as_text() {
        let mut wire = b"W (236) serial: up\r\n".to_vec();
        wire.extend(encode_record(1, b"status"));
        wire.extend(b"done\n".to_vec());
        let (events, text) = decode(&wire);
        assert_eq!(events, vec![Event::Record(1, Vec::from(b"status"))]);
        // the text on both sides of the record is retained, in order
        assert_eq!(text, b"W (236) serial: up\r\ndone\n");
    }

    #[test]
    fn a_doubled_opening_flag_is_not_a_record() {
        let wire: Vec<u8> = [FLAG, FLAG].iter().copied().chain(encode_record(2, b"ok")).collect();
        let (events, _) = decode(&wire);
        assert_eq!(events, vec![Event::Record(2, Vec::from(b"ok"))]);
    }

    #[test]
    fn an_unclosed_record_is_not_delivered() {
        let mut wire = encode_record(2, b"half");
        wire.pop();
        let (events, _) = decode(&wire);
        assert!(events.is_empty());
    }
}
