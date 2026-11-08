//! One TCP connection carrying the same bounded envelope and raw stream body.
use crate::files::Files;
use xip_abi::{
    cbor, CallView, Hw, CALL_STREAM, ERR_AGAIN, ERR_ARG, ERR_SIZE, STREAM_CLOSED, STREAM_DATA,
    STREAM_FIN, STREAM_OPEN,
};
pub struct Tcp {
    socket: u32,
    packet: u32,
    at: usize,
    len: usize,
    header: [u8; 512],
    used: usize,
    method: i32,
    fin: bool,
    remaining: u32,
    error: i32,
    error_sent: usize,
    last: u64,
}
fn call(socket: u32, method: i32, value: i32, data: &[u8]) -> CallView<'_> {
    CallView {
        component: 44,
        method,
        value,
        flags: CALL_STREAM,
        id: socket as u64,
        data,
        from: 0,
        pkt: 0,
        time_us: 0,
        request_id: None,
        src: None,
    }
}
impl Tcp {
    pub fn new(hw: Hw, socket: u32) -> Result<Self, i32> {
        Ok(Self {
            socket,
            packet: hw.pkt_alloc()?,
            at: 0,
            len: 0,
            header: [0; 512],
            used: 0,
            method: 0,
            fin: false,
            remaining: 0,
            error: 0,
            error_sent: 0,
            last: hw.now_ms(),
        })
    }
    pub fn socket(&self) -> u32 {
        self.socket
    }
    pub fn pump(&mut self, hw: Hw, files: &mut Files) -> Result<bool, i32> {
        if hw.now_ms().saturating_sub(self.last) > 60000 {
            return Err(xip_abi::ERR_TIMEOUT);
        }
        if files.tcp_done() {
            return Ok(true);
        }
        if self.fin {
            if self.error < 0 {
                let mut buf = [0; 16];
                let mut w = cbor::Writer::new(&mut buf);
                w.map(1).uint(7).int(self.error as i64);
                let b = w.finish().ok_or(ERR_SIZE)?;
                let n = hw.socket_write(self.socket, &b[self.error_sent..]);
                if n == ERR_AGAIN {
                    return Ok(false);
                }
                if n < 0 {
                    return Err(n);
                }
                self.error_sent += n as usize;
                return Ok(self.error_sent == b.len());
            }
            return Ok(false);
        }
        let (ptr, cap) = hw.pkt_buf(self.packet)?;
        if self.at == self.len {
            let n = hw.socket_read(self.socket, unsafe {
                core::slice::from_raw_parts_mut(ptr, cap)
            });
            if n == ERR_AGAIN {
                return Ok(false);
            }
            if n < 0 {
                return Err(n);
            }
            if n == 0 {
                if self.method == 0 {
                    return Err(ERR_ARG);
                }
                self.fin = true;
                if self.error < 0 {
                    return Ok(false);
                }
                let rc = files.handle_tcp(hw, &call(self.socket, self.method, STREAM_FIN, &[]));
                return if rc < 0 { Err(rc) } else { Ok(false) };
            }
            self.at = 0;
            self.len = n as usize;
            self.last = hw.now_ms();
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr, self.len) };
        while self.method == 0 && self.at < self.len {
            if self.used == self.header.len() {
                return Err(ERR_SIZE);
            }
            self.header[self.used] = bytes[self.at];
            self.used += 1;
            self.at += 1;
            let h = &self.header[..self.used];
            let mut r = cbor::Reader::new(h);
            if !r.skip() || !r.rest().is_empty() {
                continue;
            }
            if cbor::get_int(h, 1) != Ok(Some(44)) {
                return Err(ERR_ARG);
            }
            let m = cbor::get_int(h, 2).map_err(|_| ERR_ARG)?.ok_or(ERR_ARG)?;
            if !matches!(m, 2..=15 | 23) {
                return Err(ERR_ARG);
            }
            let fields = cbor::get(h, 5).map_err(|_| ERR_ARG)?;
            let data = fields.as_ref().map_or(&[0xa0][..], |r| r.rest());
            let mut r = cbor::Reader::new(data);
            if !matches!(r.next(), Some(cbor::Item::Map(_))) {
                return Err(ERR_ARG);
            }
            self.remaining = if matches!(m, 4 | 10 | 13 | 14) {
                u32::try_from(
                    cbor::get_int(data, 2)
                        .map_err(|_| ERR_ARG)?
                        .ok_or(ERR_ARG)?,
                )
                .map_err(|_| ERR_SIZE)?
            } else {
                0
            };
            let rc = files.handle_tcp(hw, &call(self.socket, m as i32, STREAM_OPEN, data));
            self.method = m as i32;
            if rc < 0 {
                self.error = rc;
            }
        }
        if self.error < 0 {
            let n = (self.len - self.at).min(self.remaining as usize);
            self.at += n;
            self.remaining -= n as u32;
        } else if self.method != 0 && self.at < self.len {
            let rc = files.handle_tcp(
                hw,
                &call(self.socket, self.method, STREAM_DATA, &bytes[self.at..]),
            );
            if rc < 0 {
                return Err(rc);
            }
            if rc as usize > self.len - self.at {
                return Err(ERR_ARG);
            }
            self.at += rc as usize;
            self.remaining = self.remaining.checked_sub(rc as u32).ok_or(ERR_SIZE)?;
        }
        // TCP is length-delimited: the API already declares the body size.
        // Do not require a peer half-close while the asynchronous flash job is
        // still using the socket. Runtime/QUIC providers continue to send FIN.
        if self.method != 0 && self.remaining == 0 {
            self.fin = true;
            if self.error == 0 {
                let rc = files.handle_tcp(hw, &call(self.socket, self.method, STREAM_FIN, &[]));
                if rc < 0 {
                    return Err(rc);
                }
            }
        }
        Ok(false)
    }
    pub fn stop(&mut self, hw: Hw, files: Option<&mut Files>) {
        if let Some(files) = files {
            if self.method != 0 {
                files.handle_tcp(hw, &call(self.socket, self.method, STREAM_CLOSED, &[]));
            }
        }
        hw.pkt_free(self.packet);
        hw.socket_close(self.socket);
    }
}
