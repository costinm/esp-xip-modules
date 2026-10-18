//! PPP-style framing of the serial line, from DMesh's `uart-codec`: records between two flags, `0x7d` escaping, and the first byte of a record is
//! its **port**. Anything outside a record is text: newline-delimited lines for the console (port 1). A record starts after a flag that is seen
//! outside a record, and its closing flag ends it; frames are written `flag port bytes flag`, so two frames in a row have two flags between them
//! and a byte that is not a flag after a closing flag is text again (a boot log, a person typing).
//!
//! The decoder writes the bytes of a record straight into the buffer of the receiver (a pool packet), as DMesh does, through a [`Sink`].
//! C runtime: not used there, it keeps the line editor of `xip_serial.c`.

pub const FLAG: u8 = 0x7e;
pub const ESCAPE: u8 = 0x7d;
pub const XOR: u8 = 0x20;
/// The port of text lines.
pub const PORT_CONSOLE: u8 = 1;
const LINE_MAX: usize = 160;

/// Where decoded records go.
pub trait Sink {
    /// Room for a record of `port`: false when no module has the port or there is no packet (the record is skipped).
    fn begin(&mut self, port: u8) -> bool;
    /// The room `begin` gave.
    fn buffer(&mut self) -> &mut [u8];
    /// The record is `len` bytes.
    fn commit(&mut self, len: usize);
    /// Never mind.
    fn abort(&mut self);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside a record: text.
    Text,
    /// After an opening flag, before the port byte.
    Start,
    /// In the bytes of a record.
    Body,
    /// Skipping to the next flag (too long, malformed, nobody listens).
    Discard,
}

pub struct Decoder {
    state: State,
    escaped: bool,
    len: usize,
    line: [u8; LINE_MAX],
    line_len: usize,
    /// Records dropped for being too long, malformed, or having no room.
    pub dropped: u32,
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder { state: State::Text, escaped: false, len: 0, line: [0; LINE_MAX], line_len: 0, dropped: 0 }
    }

    /// The input was flushed or lost bytes: forget the partial record and line.
    pub fn reset<S: Sink>(&mut self, sink: &mut S) {
        if self.state == State::Body {
            sink.abort();
        }
        self.state = State::Text;
        self.escaped = false;
        self.len = 0;
        self.line_len = 0;
    }

    fn unescape(&mut self, byte: u8) -> Option<u8> {
        if self.escaped {
            self.escaped = false;
            Some(byte ^ XOR)
        } else if byte == ESCAPE {
            self.escaped = true;
            None
        } else {
            Some(byte)
        }
    }

    fn text<S: Sink>(&mut self, byte: u8, sink: &mut S) {
        if byte == b'\r' || byte == b'\n' {
            if self.line_len == 0 {
                return;
            }
            let n = self.line_len;
            self.line_len = 0;
            if sink.begin(PORT_CONSOLE) && sink.buffer().len() >= n {
                sink.buffer()[..n].copy_from_slice(&self.line[..n]);
                sink.commit(n);
            } else {
                sink.abort();
            }
        } else if (byte == 0x08 || byte == 0x7f) && self.line_len > 0 {
            self.line_len -= 1;
        } else if byte >= b' ' && byte != 0x7f && self.line_len < LINE_MAX - 1 {
            self.line[self.line_len] = byte;
            self.line_len += 1;
        }
    }

    pub fn push<S: Sink>(&mut self, bytes: &[u8], sink: &mut S) {
        for &byte in bytes {
            if byte == FLAG {
                match self.state {
                    State::Text | State::Discard => {
                        self.state = State::Start;
                        self.line_len = 0;
                    }
                    State::Start => {} // a doubled opening flag
                    State::Body => {
                        if self.escaped || self.len == 0 {
                            sink.abort();
                            self.dropped += 1;
                        } else {
                            sink.commit(self.len);
                        }
                        self.state = State::Text;
                    }
                }
                self.escaped = false;
                self.len = 0;
                continue;
            }
            match self.state {
                State::Text => self.text(byte, sink),
                State::Discard => {}
                State::Start => {
                    let Some(port) = self.unescape(byte) else { continue };
                    if port != 0 && sink.begin(port) {
                        self.state = State::Body;
                        self.len = 0;
                    } else {
                        self.state = State::Discard;
                    }
                }
                State::Body => {
                    let Some(b) = self.unescape(byte) else { continue };
                    if self.len < sink.buffer().len() {
                        let at = self.len;
                        sink.buffer()[at] = b;
                        self.len += 1;
                    } else {
                        sink.abort();
                        self.dropped += 1;
                        self.state = State::Discard;
                    }
                }
            }
        }
    }
}

/// Write one record for `port`: `flag port bytes flag`, escaped, in pieces of at most 64 bytes through `out`. Port 1 is text as it is.
pub fn encode(port: u8, data: &[u8], mut out: impl FnMut(&[u8])) {
    if port == PORT_CONSOLE {
        out(data);
        return;
    }
    let mut buf = [0u8; 64];
    let mut n = 0;
    let mut put = |b: u8, n: &mut usize, out: &mut dyn FnMut(&[u8])| {
        buf[*n] = b;
        *n += 1;
        if *n == buf.len() {
            out(&buf[..*n]);
            *n = 0;
        }
    };
    let out: &mut dyn FnMut(&[u8]) = &mut out;
    put(FLAG, &mut n, out);
    for b in core::iter::once(port).chain(data.iter().copied()) {
        if b == FLAG || b == ESCAPE {
            put(ESCAPE, &mut n, out);
            put(b ^ XOR, &mut n, out);
        } else {
            put(b, &mut n, out);
        }
    }
    put(FLAG, &mut n, out);
    if n > 0 {
        out(&buf[..n]);
    }
}

#[cfg(target_os = "espidf")]
mod esp {
    use super::*;
    use core::ffi::c_int;

    #[repr(C)]
    struct RxBuf {
        data: *mut u8,
        capacity: usize,
        slot: c_int,
    }
    #[repr(C)]
    struct Codec {
        on_bytes: Option<unsafe extern "C" fn(*const u8, usize)>,
        on_reset: Option<unsafe extern "C" fn()>,
        tx: Option<unsafe extern "C" fn(u32, *const u8, usize) -> c_int>,
    }
    extern "C" {
        fn xip_serial_rx_begin(port: u32, buf: *mut RxBuf) -> c_int;
        fn xip_serial_rx_commit(port: u32, buf: *const RxBuf, len: usize);
        fn xip_serial_rx_abort(buf: *const RxBuf);
        fn xip_serial_write(data: *const u8, len: usize) -> c_int;
        fn xip_serial_set_codec(codec: *const Codec);
    }

    /// The receive buffers of the runtime: a pool packet per record.
    struct Pool {
        buf: RxBuf,
        port: u32,
        open: bool,
    }

    impl Sink for Pool {
        fn begin(&mut self, port: u8) -> bool {
            if self.open {
                self.abort();
            }
            self.port = port as u32;
            self.open = unsafe { xip_serial_rx_begin(self.port, &mut self.buf) } == 0;
            self.open
        }
        fn buffer(&mut self) -> &mut [u8] {
            if self.open {
                unsafe { core::slice::from_raw_parts_mut(self.buf.data, self.buf.capacity) }
            } else {
                &mut []
            }
        }
        fn commit(&mut self, len: usize) {
            if self.open {
                unsafe { xip_serial_rx_commit(self.port, &self.buf, len) };
                self.open = false;
            }
        }
        fn abort(&mut self) {
            if self.open {
                unsafe { xip_serial_rx_abort(&self.buf) };
                self.open = false;
            }
        }
    }

    // Only the serial reader task touches these (the C driver calls the codec from it).
    static mut DECODER: Decoder = Decoder::new();
    static mut POOL: Pool = Pool { buf: RxBuf { data: core::ptr::null_mut(), capacity: 0, slot: -1 }, port: 0, open: false };

    unsafe extern "C" fn on_bytes(data: *const u8, len: usize) {
        let bytes = unsafe { core::slice::from_raw_parts(data, len) };
        unsafe { (*core::ptr::addr_of_mut!(DECODER)).push(bytes, &mut *core::ptr::addr_of_mut!(POOL)) };
    }

    unsafe extern "C" fn on_reset() {
        unsafe { (*core::ptr::addr_of_mut!(DECODER)).reset(&mut *core::ptr::addr_of_mut!(POOL)) };
    }

    unsafe extern "C" fn tx(port: u32, data: *const u8, len: usize) -> c_int {
        let bytes = unsafe { core::slice::from_raw_parts(data, len) };
        encode(port as u8, bytes, |chunk| {
            unsafe { xip_serial_write(chunk.as_ptr(), chunk.len()) };
        });
        len as c_int
    }

    static CODEC: Codec = Codec { on_bytes: Some(on_bytes), on_reset: Some(on_reset), tx: Some(tx) };
    unsafe impl Sync for Codec {}

    /// Give the serial driver this framing: ports in PPP records, text lines for the console.
    pub fn install() {
        unsafe { xip_serial_set_codec(&CODEC) };
    }
}

#[cfg(target_os = "espidf")]
pub use esp::install;

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::{vec, vec::Vec};

    #[derive(Default)]
    struct Collect {
        listening: Vec<u8>,
        room: Vec<u8>,
        open: bool,
        got: Vec<(u8, Vec<u8>)>,
        port: u8,
    }
    impl Sink for Collect {
        fn begin(&mut self, port: u8) -> bool {
            self.open = self.listening.contains(&port);
            self.port = port;
            self.room = vec![0; 32];
            self.open
        }
        fn buffer(&mut self) -> &mut [u8] {
            &mut self.room
        }
        fn commit(&mut self, len: usize) {
            self.got.push((self.port, self.room[..len].to_vec()));
            self.open = false;
        }
        fn abort(&mut self) {
            self.open = false;
        }
    }

    fn wire(port: u8, data: &[u8]) -> Vec<u8> {
        let mut w = Vec::new();
        encode(port, data, |c| w.extend_from_slice(c));
        w
    }

    #[test]
    fn a_record_carries_its_port_and_escapes_the_flag() {
        let mut sink = Collect { listening: vec![2, 0xf7], ..Default::default() };
        let mut d = Decoder::new();
        let payload = [1, FLAG, ESCAPE, 9];
        let w = wire(2, &payload);
        assert_eq!(w[0], FLAG);
        assert_eq!(*w.last().unwrap(), FLAG);
        assert!(!w[1..w.len() - 1].contains(&FLAG));
        d.push(&w, &mut sink);
        assert_eq!(sink.got, vec![(2, payload.to_vec())]);
    }

    #[test]
    fn records_in_a_row_and_in_pieces() {
        let mut sink = Collect { listening: vec![2, 3], ..Default::default() };
        let mut d = Decoder::new();
        let mut w = wire(2, b"one");
        w.extend(wire(3, b"two"));
        for piece in w.chunks(1) {
            d.push(piece, &mut sink);
        }
        assert_eq!(sink.got, vec![(2, b"one".to_vec()), (3, b"two".to_vec())]);
    }

    #[test]
    fn text_between_records_is_the_console() {
        let mut sink = Collect { listening: vec![1, 2], ..Default::default() };
        let mut d = Decoder::new();
        d.push(b"status\r\n", &mut sink);
        d.push(&wire(2, b"x"), &mut sink);
        d.push(b"he\x08elp\n\n", &mut sink); // backspace edits, empty lines are dropped
        assert_eq!(sink.got, vec![(1, b"status".to_vec()), (2, b"x".to_vec()), (1, b"help".to_vec())]);
    }

    #[test]
    fn the_record_of_a_port_nobody_has_is_skipped_and_the_next_is_not() {
        let mut sink = Collect { listening: vec![3], ..Default::default() };
        let mut d = Decoder::new();
        let mut w = wire(9, b"lost");
        w.extend(wire(3, b"kept"));
        d.push(&w, &mut sink);
        assert_eq!(sink.got, vec![(3, b"kept".to_vec())]);
    }

    #[test]
    fn too_long_and_empty_records_are_dropped() {
        let mut sink = Collect { listening: vec![2], ..Default::default() };
        let mut d = Decoder::new();
        let mut w = wire(2, &[7; 40]);
        w.extend(wire(2, b""));
        w.extend(wire(2, b"ok"));
        d.push(&w, &mut sink);
        assert_eq!(sink.got, vec![(2, b"ok".to_vec())]);
        assert_eq!(d.dropped, 2);
    }

    #[test]
    fn console_text_is_written_as_it_is() {
        assert_eq!(wire(1, b"ok\n"), b"ok\n".to_vec());
    }
}
