//! One report sink using the runtime's transport-neutral stream interface.
//! The runtime dispatches the opening request; NAN never parses stream input.
use xip_abi::*;

pub struct Session { pub handle: u32 }
impl Session {
    /// The stream OPEN handler registers its opaque provider-independent handle.
    pub fn new(handle: u32) -> Self { Self { handle } }
    /// NAN observation delivery sends a live report body; a blocked output drops this observation.
    pub fn report(&self, hw: &Hw, data: &[u8]) -> Result<(), i32> {
        let write=hw.stream().and_then(|s|s.write_pkt).ok_or(ERR_UNSUPPORTED)?;
        let packet=hw.pkt_alloc()?;
        let result=(|| {
            let (p,cap)=hw.pkt_buf(packet)?;
            if data.len()>cap {return Err(ERR_SIZE);}
            unsafe {core::ptr::copy_nonoverlapping(data.as_ptr(),p,data.len());}
            let rc=unsafe {write(hw.ctx(),self.handle,packet,data.len() as u32,0)};
            if rc<0 {Err(rc)} else {Ok(())}
        })();
        // Success transfers ownership; every error retains the exact packet.
        if result.is_err() {hw.pkt_free(packet);}
        result
    }
    /// Module stop or protocol rejection closes the stream and releases native output.
    pub fn dispose(self, hw: &Hw) {
        if let Some(close)=hw.stream().and_then(|s|s.close) {
            unsafe {close(hw.ctx(),self.handle);}
        }
    }
}
