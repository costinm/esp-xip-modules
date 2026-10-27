//! Borrow stream bytes for one runtime call; QUIC owns any refused suffix.
use super::*;
pub(crate) struct Session {
    pub stream: QuicStream,
    pub input: Receiver,
    pub output: Option<OwnedPacket<ErasedPacketBuffer>>,
    pub output_fin: bool,
    pub output_at: usize,
    pub closed: bool,
}
impl Session {
    pub fn new(stream: QuicStream) -> Self {
        Self {
            stream,
            input: Receiver::default(),
            output: None,
            output_fin: false,
            output_at: 0,
            closed: false,
        }
    }
    pub fn close(&mut self, native: Native, table: &Stream) {
        if self.input.handle != 0 {
            if let Some(f) = table.close {
                unsafe {
                    f(native.hw.ctx(), self.input.handle);
                }
            }
        }
        self.closed = true;
        self.input.needs_resume = false;
        self.output = None;
    }
}

#[derive(Default)]
pub(crate) struct Receiver {
    pub handle: u32,
    pub needs_resume: bool,
    pub input_error: Option<i32>,
    fin: bool,
}
impl Receiver {
    pub fn feed(&mut self, native: Native, table: &Stream, bytes: &[u8], fin: bool) -> Result<usize, i32> {
        if self.fin { return Err(ERR_ARG); }
        if self.handle == 0 {
            // Runtime owns framing/envelope dispatch. No borrowed pointer escapes.
            let rc = unsafe {
                table.open.ok_or(ERR_UNSUPPORTED)?(native.hw.ctx(), core::ptr::null(), 0)
            };
            if rc <= 0 { return Err(if rc < 0 {rc} else {ERR_IO}); }
            self.handle = rc as u32;
        }
        let n = if bytes.is_empty() { 0 } else { unsafe {
            table.feed.ok_or(ERR_UNSUPPORTED)?(
                native.hw.ctx(), self.handle, bytes.as_ptr(), bytes.len(), 0,
            )
        }};
        if n < 0 { return Err(n); }
        let n = n as usize;
        if n > bytes.len() { return Err(ERR_ARG); }
        self.needs_resume = n < bytes.len();
        if !self.needs_resume && fin {
            let rc = unsafe {
                table.feed.ok_or(ERR_UNSUPPORTED)?(
                    native.hw.ctx(), self.handle, core::ptr::null(), 0, 1,
                )
            };
            if rc < 0 { return Err(rc); }
            self.fin = true;
        }
        Ok(n)
    }
}
