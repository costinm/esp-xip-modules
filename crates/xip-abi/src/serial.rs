//! Native C serial adapter contract. C owns framing and accepted packet lifetimes;
//! callbacks bridge an application's pool and typed consumer without copying bytes.
use core::ffi::{c_int, c_void};

/// Writable packet returned by the pool adapter. A successful begin transfers
/// one writer to C, which must call rx_finish exactly once (commit or abort).
#[repr(C)]
pub struct SerialRxBuffer {
    pub data: *mut u8,
    pub capacity: usize,
}

/// One consumed TX packet. C borrows its bytes until complete, then calls the
/// single-use destructor/completion callback on success, error or teardown.
#[repr(C)]
pub struct SerialPacketTx {
    pub data: *const u8,
    pub len: usize,
    pub port: u32,
    pub owner: *mut c_void,
    pub complete: Option<unsafe extern "C" fn(owner: *mut c_void, outcome: c_int)>,
}

/// Task-context pool/consumer bridge for the shared C PPP codec. ctx is consumed
/// by start on every outcome; close runs after callbacks and packets quiesce.
#[repr(C)]
pub struct SerialPacketClient {
    pub ctx: *mut c_void,
    /// 0: one exclusive writer returned; positive: unbound port; negative: drop.
    pub rx_begin: Option<unsafe extern "C" fn(ctx: *mut c_void, port: u32, buffer: *mut SerialRxBuffer) -> c_int>,
    /// outcome 0 commits len bytes to the consumer; negative aborts the writer.
    pub rx_finish: Option<unsafe extern "C" fn(ctx: *mut c_void, port: u32, len: usize, outcome: c_int)>,
    /// Events: 1 bytes, 2 driver wake, 3 reset, 4 packet, 5 wake, 6 pool drop,
    /// 7 TX ready. Value is a count (bytes for event 1).
    pub event: Option<unsafe extern "C" fn(ctx: *mut c_void, event: u32, value: u32)>,
    pub close: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
}
