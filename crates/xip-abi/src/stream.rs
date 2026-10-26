//! Transport-neutral module streams. Calls are serialized; DATA is borrowed only
//! during the handler. Its nonnegative return is the exact consumed byte count.
use core::ffi::{c_int, c_void};
pub const STREAM_VERSION: u32 = 1;
pub const CALL_STREAM: u32 = 16;
pub const STREAM_OPEN: i32 = 1;
pub const STREAM_DATA: i32 = 2;
pub const STREAM_FIN: i32 = 3;
pub const STREAM_CLOSED: i32 = 4;
#[repr(C)]
#[derive(Default)]
pub struct StreamPacket {
    pub packet: u32,
    pub len: u32,
    pub fin: u32,
}
/// Native `get_component("stream")`. Handles bind provider and consumer instance
/// generations; stopping either invalidates them. No body queue lives here.
#[repr(C)]
pub struct Stream {
    pub version: u32,
    pub size: u32,
    /// Consumer binds an envelope component/method for this instance. Released
    /// on stop. Primary components with a stream manifest need no explicit bind.
    pub listen:
        Option<unsafe extern "C" fn(ctx: *mut c_void, component: c_int, method: c_int) -> c_int>,
    /// Provider opens from one complete CBOR request envelope; returns handle.
    pub open:
        Option<unsafe extern "C" fn(ctx: *mut c_void, envelope: *const u8, len: usize) -> c_int>,
    /// Provider delivers borrowed bytes. fin requires len=0 and follows all
    /// consumed body bytes. Partial/zero consumption retains bytes at provider.
    pub feed: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            handle: u32,
            data: *const u8,
            len: usize,
            fin: u32,
        ) -> c_int,
    >,
    /// Either endpoint closes; peer receives CLOSED. Idempotent only for a live handle.
    pub close: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: u32) -> c_int>,
    /// Consumer transfers one pool packet on OK; EVERY error retains it.
    /// One outstanding output per stream. fin half-closes after these bytes.
    pub write_pkt: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            handle: u32,
            packet: u32,
            len: u32,
            fin: u32,
        ) -> c_int,
    >,
    /// Provider takes the exact pending physical slot, or gets 0 if none.
    /// A full held quota leaves it pending for the next poll.
    pub take: Option<
        unsafe extern "C" fn(ctx: *mut c_void, handle: u32, out: *mut StreamPacket) -> c_int,
    >,
    /// Provider accepted the outstanding packet into reliable history.
    pub written: Option<unsafe extern "C" fn(ctx: *mut c_void, handle: u32) -> c_int>,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Stream>() == 36);

const _: () = assert!(core::mem::size_of::<StreamPacket>() == 12);
