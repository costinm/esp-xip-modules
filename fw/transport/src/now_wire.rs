//! Product NOW adapter bindings to the shared C wire codec.
extern "C" {
    fn xip_espnow_frame_encode(
        out: *mut u8,
        cap: usize,
        dst: *const u8,
        src: *const u8,
        bssid: *const u8,
        data: *const u8,
        len: usize,
    ) -> usize;
    fn xip_espnow_body_decode(body: *const u8, len: usize, out: *mut u8, cap: usize) -> isize;
    fn xip_espnow_frame_decode(
        frame: *const u8,
        len: usize,
        source: *mut u8,
        out: *mut u8,
        cap: usize,
    ) -> isize;
}
pub fn encode_action_frame(
    out: &mut [u8],
    dst: [u8; 6],
    src: [u8; 6],
    bssid: [u8; 6],
    data: &[u8],
) -> Result<usize, i32> {
    let n = unsafe {
        xip_espnow_frame_encode(
            out.as_mut_ptr(),
            out.len(),
            dst.as_ptr(),
            src.as_ptr(),
            bssid.as_ptr(),
            data.as_ptr(),
            data.len(),
        )
    };
    if n == 0 {
        Err(xip_abi::ERR_SIZE)
    } else {
        Ok(n)
    }
}
pub fn parse_action_body_into(body: &[u8], out: &mut [u8]) -> Option<usize> {
    let n =
        unsafe { xip_espnow_body_decode(body.as_ptr(), body.len(), out.as_mut_ptr(), out.len()) };
    (n >= 0).then_some(n as usize)
}
pub fn parse_action_frame_into(frame: &[u8], out: &mut [u8]) -> Option<([u8; 6], usize)> {
    let mut source = [0; 6];
    let n = unsafe {
        xip_espnow_frame_decode(
            frame.as_ptr(),
            frame.len(),
            source.as_mut_ptr(),
            out.as_mut_ptr(),
            out.len(),
        )
    };
    (n >= 0).then_some((source, n as usize))
}
