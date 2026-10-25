//! The `nimble` component: a thin subset of the NimBLE host, from `get_component("nimble")`. Advertising with the data the caller supplies, passive
//! scan with the caller's filters, one LE CoC channel (listen or connect, one SDU per packet), the bond store and the wake/sleep modes; the
//! stack is started by the first `start`.
//!
//! The component holds no product policy: which service UUIDs are advertised and how the advertisement is laid out, its intervals, the device
//! name, the PSM, what a scan reports as a match, the SDU size and the buffer count of the CoC pool are all arguments, chosen by the module
//! that owns the policy (`mod_ble`, which hosts the `ble` component, 104). The CoC pool is sized by the first `start` and stays.
//! The status, scan results and mode types are the ones of the `ble` API ([`BleStatus`], [`BleScanResults`], [`BleMode`]): they carry no policy.
//!
//! The table's functions take `ctx` first and never block; the real work runs on the NimBLE host task and is queued from the caller. The
//! component is optional: a host built without BLE (`CONFIG_XIP_BLE=n`) returns no table.
use core::ffi::{c_int, c_void};

use crate::ble::{BleMode, BleScanResults, BleStatus};

/// `Nimble::version`.
pub const NIMBLE_VERSION: u32 = 1;
/// Bytes of advertising data or scan response data (the legacy PDU limit).
pub const NIMBLE_ADV_MAX: usize = 31;
/// Bytes of the GAP device name, including none for a terminator.
pub const NIMBLE_NAME_MAX: usize = 16;
/// Scan filters of a [`NimbleScan`].
pub const NIMBLE_FILTERS: usize = 3;

/// One scan filter: an advertisement matches when it has an AD structure of type `ad_types[0]` or `ad_types[1]` (0: unused) whose payload,
/// read as elements of `len` bytes (2 for a 16-bit UUID list, 16 for a 128-bit one), has one equal to `data[..len]`. The match is the flag
/// bit 0 of the scan result and counts in `scan_matches`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NimbleFilter {
    pub ad_types: [u8; 2],
    /// Bytes of `data` that are compared; 0 turns the filter off.
    pub len: u8,
    pub reserved: u8,
    pub data: [u8; 16],
}

/// Start the stack when needed, name the device, size the CoC pool, serve a CoC PSM and advertise: for [`Nimble::start`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NimbleStart {
    /// `sizeof(xip_nimble_start)`, set by the caller.
    pub size: u32,
    /// The LE PSM of the CoC server (0x0080..0x00ff); 0: no server.
    pub psm: u16,
    /// SDU size of the CoC channel and the buffers of its pool, fixed by the first start (up to `CONFIG_XIP_BLE_COC_MTU`).
    pub coc_mtu: u16,
    /// Buffers of the CoC pool (up to `CONFIG_XIP_BLE_COC_BUFS`).
    pub coc_buffers: u8,
    /// Bytes of `adv` to advertise; 0: do not advertise (a server only).
    pub adv_len: u8,
    /// Bytes of `scan_rsp`; 0: none.
    pub scan_rsp_len: u8,
    /// Bytes of `name`; 0: leave the name as it is.
    pub name_len: u8,
    /// Advertising interval in units of 0.625 ms; `adv_max` below `adv_min` is `adv_min`.
    pub adv_min: u16,
    pub adv_max: u16,
    pub adv: [u8; 31],
    pub scan_rsp: [u8; 31],
    /// The GAP device name.
    pub name: [u8; 16],
}
impl Default for NimbleStart {
    fn default() -> Self {
        let mut s: NimbleStart = unsafe { core::mem::zeroed() };
        s.size = core::mem::size_of::<NimbleStart>() as u32;
        s
    }
}

/// Passive scan with duplicates filtered, for [`Nimble::scan`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NimbleScan {
    pub size: u32,
    /// Milliseconds, as given (the caller decides defaults and limits).
    pub duration_ms: u32,
    pub filters: [NimbleFilter; 3],
}
impl Default for NimbleScan {
    fn default() -> Self {
        let mut s: NimbleScan = unsafe { core::mem::zeroed() };
        s.size = core::mem::size_of::<NimbleScan>() as u32;
        s
    }
}

/// Central connect and open the CoC channel, for [`Nimble::connect`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NimbleConnect {
    pub size: u32,
    /// The peer, as the stack reports it (little-endian, the order scan results use).
    pub addr: [u8; 6],
    /// 0 public, 1 random.
    pub addr_type: u8,
    pub reserved: u8,
    /// The peer's LE PSM (0x0080..0x00ff).
    pub psm: u16,
    /// SDU size for this channel, at most the pool's; 0: the pool's.
    pub coc_mtu: u16,
    pub reserved2: [u8; 2],
}
impl Default for NimbleConnect {
    fn default() -> Self {
        let mut s: NimbleConnect = unsafe { core::mem::zeroed() };
        s.size = core::mem::size_of::<NimbleConnect>() as u32;
        s
    }
}

/// The `nimble` component table, from `get_component("nimble")`; absent on a host without BLE. `ctx` first, never blocks, valid for the firmware
/// lifetime. The order of the members is part of the ABI.
#[repr(C)]
pub struct Nimble {
    /// `NIMBLE_VERSION`.
    pub version: u32,
    /// `sizeof(xip_nimble)`: members past it do not exist.
    pub size: u32,
    /// Start the stack when needed, set the name, create the CoC pool and server and advertise (see [`NimbleStart`]). The advertisement is restarted after every GAP disconnect while it is wanted.
    pub start: Option<unsafe extern "C" fn(ctx: *mut c_void, request: *const NimbleStart) -> c_int>,
    /// Stop advertising, scanning and the connection (disconnect 0x13).
    pub stop: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// Scan for the duration; `-110` (timed out) before the stack is ready.
    pub scan: Option<unsafe extern "C" fn(ctx: *mut c_void, request: *const NimbleScan) -> c_int>,
    /// Read the state (set `status.size` first).
    pub status: Option<unsafe extern "C" fn(ctx: *mut c_void, status: *mut BleStatus) -> c_int>,
    /// Stop a running scan.
    pub scan_stop: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// Connect as a central to `addr` and open the CoC channel on `psm`; a scan in progress is cancelled first.
    pub connect: Option<unsafe extern "C" fn(ctx: *mut c_void, request: *const NimbleConnect) -> c_int>,
    /// Forget every stored bond (a bonded peer must pair again).
    pub bonds_clear: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// Set the wake/sleep mode and the boot flag.
    pub mode: Option<unsafe extern "C" fn(ctx: *mut c_void, request: *const BleMode) -> c_int>,
    /// The scan cache (set `results.size` first); empty when the cache is built out.
    pub scan_results: Option<unsafe extern "C" fn(ctx: *mut c_void, results: *mut BleScanResults) -> c_int>,
    /// Before a light sleep: in rendezvous mode stop advertising and scanning. `ERR_BUSY` when a link is up (or in always mode with the service live): the sleep is vetoed.
    pub suspend: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// After a light sleep: re-arm what `suspend` stopped.
    pub resume: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
}

const _: () = assert!(core::mem::size_of::<NimbleFilter>() == 20);
const _: () = assert!(core::mem::size_of::<NimbleStart>() == 96);
const _: () = assert!(core::mem::size_of::<NimbleScan>() == 68);
const _: () = assert!(core::mem::size_of::<NimbleConnect>() == 20);
