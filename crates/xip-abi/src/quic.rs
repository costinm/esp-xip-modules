//! Registration of a local QUIC endpoint. Packet allocation and TX use Host.
use core::ffi::{c_int,c_void};
pub const QUIC_VERSION: u32 = 2;
/// get_component("quic"), native component 13. Functions run on the module worker.
#[repr(C)]
pub struct Quic {
    pub version: u32,
    pub size: u32,
    /// Attach this instance as the one local/unhandled endpoint. RX is delivered
    /// as CALL_LINK(component=this module, method=rx_method), preserving origin.
    pub attach: Option<unsafe extern "C" fn(ctx: *mut c_void, rx_method: c_int) -> c_int>,
    /// Stop admission. The module closes its protocol holders; accepted native TX
    /// references finish independently without calling the stopped module.
    pub detach: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    /// Atomically create (mode=0) or replace (mode=1) a forwarding owned by this instance.
    pub relay_set: Option<unsafe extern "C" fn(ctx: *mut c_void, route: *const RelayRoute, mode: u32) -> c_int>,
    /// Read this instance's route at a sorted index. NOTFOUND ends enumeration.
    pub relay_get: Option<unsafe extern "C" fn(ctx: *mut c_void, index: u32, route: *mut RelayRoute) -> c_int>,
    pub relay_remove: Option<unsafe extern "C" fn(ctx: *mut c_void, cid: *const u8, len: usize) -> c_int>,
    /// Allocate an opaque CID, or reuse an identical live owned forwarding.
    /// Returns 1 for reuse, 0 for a new route; negative errors leave routes intact.
    pub relay_create: Option<unsafe extern "C" fn(ctx: *mut c_void, route: *mut RelayRoute, idle_ms: u32, reuse: u32) -> c_int>,
    pub relay_stats: Option<unsafe extern "C" fn(ctx: *mut c_void, cid: *const u8, len: usize, stats: *mut RelayStats) -> c_int>,
    /// Remove expired owned routes; returns the number removed.
    pub relay_expire: Option<unsafe extern "C" fn(ctx: *mut c_void) -> c_int>,
    pub relay_idle: Option<unsafe extern "C" fn(ctx: *mut c_void, cid: *const u8, len: usize, idle_ms: u32) -> c_int>,
 }
/// One weighted egress. Destination is the link's opaque peer token (MACs use mac_token).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayHop {
    pub token: u64,
    pub link: u32,
    pub weight: u16,
    pub cid_len: u8,
    /// 0 uses token; 22 is IPv6 (IPv4 mapped), big-endian port and scope.
    pub address_len: u8,
    pub rewritten_dcid: [u8; 20],
    pub padding: [u8; 4],
    pub address: [u8; 24],
}
/// At most four egress alternatives. Zero weights disable an alternative;
/// at least one must be positive. Selection is weighted round robin per packet.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayRoute {
    pub hops: [RelayHop; 4],
    pub dcid: [u8; 20],
    pub dcid_len: u8,
    pub hop_count: u8,
    /// 1 terminates an opaque short-header circuit; hops describe the return path.
    pub endpoint: u8,
    pub reserved: u8,
}
const _: () = assert!(core::mem::size_of::<RelayHop>() == 64);
const _: () = assert!(core::mem::size_of::<RelayRoute>() == 280);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Quic>() == 44);

/// Monotonic local observation. Configuration reads never refresh last use.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    pub last_used_us: u64,
    /// 0 disables inactivity expiry.
    pub idle_ms: u32,
    pub reserved: u32,
}
const _: () = assert!(core::mem::size_of::<RelayStats>() == 16);
