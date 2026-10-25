//! Owned packet seam for one serialized module QUIC endpoint. No protocol state lives here.
use core::ffi::{c_int, c_void};
pub const QUIC_VERSION: u32 = 1;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct QuicPoolInfo {
    /// sizeof(QuicPoolInfo).
    pub size: u32,
    /// Physical pool slot bytes, including prefix and tail.
    pub slot_bytes: u32,
    pub headroom: u32,
    pub max_packet: u32,
    /// Current endpoint lease budget, bounded by module quota and pool capacity.
    pub capacity: u32,
    pub available: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct QuicTxCompletion {
    pub size: u32,
    /// Newly owned module handle for the exact completed physical slot.
    pub packet: u32,
    pub ticket: u32,
    pub start: u32,
    pub len: u32,
    pub link: u32,
    pub peer: u64,
    pub outcome: i32,
    pub elapsed_us: u32,
}
/// get_component("quic"), native component 13. Functions run on the module worker.
#[repr(C)]
pub struct Quic {
    pub version: u32,
    pub size: u32,
    /// Attach this instance as the one local/unhandled endpoint. RX is delivered
    /// as CALL_LINK(component=this module, method=rx_method), preserving origin.
    pub attach: Option<unsafe extern "C" fn(ctx: *mut c_void, rx_method: c_int) -> c_int>,
    /// Stop admission, release completed packets, disown native physical sends.
    /// A late SDK completion releases its old epoch's lease without calling Rust.
    pub detach: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub pool_info: Option<unsafe extern "C" fn(ctx: *mut c_void, out: *mut QuicPoolInfo) -> c_int>,
    /// Allocate one owned handle while leaving reserved physical pool slots free.
    pub acquire:
        Option<unsafe extern "C" fn(ctx: *mut c_void, reserved: u32, out: *mut u32) -> c_int>,
    /// Transfer a uniquely owned range only on success. ALL immediate negative
    /// returns retain the exact handle/range (unlike consuming Host.send).
    /// Success sets ticket; poll_tx returns the same backing slot after native TX.
    pub try_send: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            packet: u32,
            start: u32,
            len: u32,
            link: u32,
            peer: u64,
            ticket: *mut u32,
        ) -> c_int,
    >,
    /// 1 completed lease, 0 none, negative error. A full held quota retains the
    /// native completion for another poll; completions are never lost to inbox pressure.
    pub poll_tx:
        Option<unsafe extern "C" fn(ctx: *mut c_void, out: *mut QuicTxCompletion) -> c_int>,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Quic>() == 32);
const _: () = assert!(core::mem::size_of::<QuicPoolInfo>() == 24);
const _: () = assert!(core::mem::size_of::<QuicTxCompletion>() == 40);
