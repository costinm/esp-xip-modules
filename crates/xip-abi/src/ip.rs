//! Experimental raw-IP virtual interface. Payloads use the existing owned link API.
use core::ffi::{c_int, c_void};
pub const IP_VERSION: u32 = 1;
/// Point-to-point IP interface. No Ethernet/ARP, default route, forwarding or
/// compression. Static IPv6 addresses use an on-link /64; all-zero disables a family.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpConfig {
    pub size: u32,
    /// 0 selects1280; otherwise1280..1500. Tunnel endpoints budget outer headers.
    pub mtu: u16,
    pub reserved: u16,
    pub ipv4: [u8; 4],
    pub netmask: [u8; 4],
    pub gateway: [u8; 4],
    pub ipv6: [u8; 16],
}
impl Default for IpConfig {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            mtu: 1280,
            reserved: 0,
            ipv4: [0; 4],
            netmask: [0; 4],
            gateway: [0; 4],
            ipv6: [0; 16],
        }
    }
}
const _: () = assert!(core::mem::size_of::<IpConfig>() == 36);
#[repr(C)]
pub struct Ip {
    pub version: u32,
    pub size: u32,
    /// Creates one lazy native link named `ip`, returning its address. The owner
    /// subscribes with Link.open and injects complete packets with normal send.
    /// Owned input is retained by lwIP until its native custom-pbuf callback
    /// releases it. No module pointer/vtable survives in the stack.
    pub open: Option<
        unsafe extern "C" fn(ctx: *mut c_void, config: *const IpConfig, link: *mut u32) -> c_int,
    >,
    pub set_up: Option<unsafe extern "C" fn(ctx: *mut c_void, link: u32, up: u32) -> c_int>,
    /// Removes the netif synchronously on the TCP/IP thread. Owner stop and
    /// network sleep teardown also close it; recreate after resume.
    pub close: Option<unsafe extern "C" fn(ctx: *mut c_void, link: u32) -> c_int>,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Ip>() == 20);
