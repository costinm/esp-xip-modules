//! Opaque stream bytes passed to the runtime; retain each chunk until consumed.
use super::*;
pub(crate) struct Input {
    pub bytes: Vec<u8>,
    pub fin: bool,
}
pub(crate) struct Session {
    pub stream: QuicStream,
    pub handle: u32,
    pub input: Option<Input>,
    at: usize,
    pub output: Option<OwnedPacket<ErasedPacketBuffer>>,
    pub output_fin: bool,
    pub output_at: usize,
    input_fin: bool,
    pub closed: bool,
}
impl Session {
    pub fn new(stream: QuicStream) -> Self {
        Self {
            stream,
            handle: 0,
            input: None,
            at: 0,
            output: None,
            output_fin: false,
            output_at: 0,
            input_fin: false,
            closed: false,
        }
    }
    pub fn feed(&mut self, native: Native, table: &Stream) -> Result<(), i32> {
        let Some(chunk) = self.input.as_ref() else {
            return Ok(());
        };
        if self.closed {
            self.input = None;
            return Ok(());
        }
        if self.input_fin {
            return Err(ERR_ARG);
        }
        if self.handle == 0 {
            // The runtime owns framing, envelope parsing and module selection.
            // This creates an unbound stream; feed passes it the original bytes.
            let rc = unsafe {
                table.open.ok_or(ERR_UNSUPPORTED)?(native.hw.ctx(), core::ptr::null(), 0)
            };
            if rc <= 0 { return Err(if rc < 0 {rc}else{ERR_IO}); }
            self.handle = rc as u32;
        }
        if self.handle != 0 {
            if self.at < chunk.bytes.len() {
                let bytes = &chunk.bytes[self.at..];
                let n = unsafe {
                    table.feed.ok_or(ERR_UNSUPPORTED)?(
                        native.hw.ctx(),
                        self.handle,
                        bytes.as_ptr(),
                        bytes.len(),
                        0,
                    )
                };
                if n < 0 {
                    return Err(n);
                }
                if n as usize > bytes.len() {
                    return Err(ERR_ARG);
                }
                self.at += n as usize;
            }
            if self.at == chunk.bytes.len() {
                if chunk.fin {
                    let rc = unsafe {
                        table.feed.ok_or(ERR_UNSUPPORTED)?(
                            native.hw.ctx(),
                            self.handle,
                            core::ptr::null(),
                            0,
                            1,
                        )
                    };
                    if rc < 0 {
                        return Err(rc);
                    }
                    self.input_fin = true;
                }
                self.input = None;
                self.at = 0;
            }
        }
        Ok(())
    }
    pub fn close(&mut self, native: Native, table: &Stream) {
        if self.handle != 0 {
            if let Some(f) = table.close {
                unsafe {
                    f(native.hw.ctx(), self.handle);
                }
            }
        }
        self.closed = true;
        self.input = None;
        self.output = None;
    }
}
