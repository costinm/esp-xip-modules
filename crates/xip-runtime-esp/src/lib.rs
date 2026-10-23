//! The Rust side of an esp-xip-modules application (the C side is `components/xip_runtime`, `xip_loader`): start the host
//! and the loader over the module partition, start, stop and call modules, receive their events.
//!
//! Generic host facilities include module lifecycle, packet-backed socket
//! readiness, settings, the `wifi` components with the C Wi-Fi owner.
//! Products keep protocol parsing and radio scheduling policy in their own owners: an application that drives the
//! radio itself passes a [`WifiOwner`] in [`Features`]. The module interface is a C-compatible function table.
//!
//! The ABI itself, for module code, is the `xip-abi` crate.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod loader;
#[cfg(target_os = "espidf")]
pub mod settings;

pub use loader::{autostart, call, find, init, manifest, request_raw, slot_manifest, slot_count, start, status, stop, stop_all, EventHandler, Features, PoolOps, SlotStatus, WifiOwner};

/// Read host status without depending on a console module.
#[cfg(target_os = "espidf")]
pub fn device_info(out: &mut [u8]) -> Result<usize, i32> {
    extern "C" { fn xip_device_info(out: *mut u8, capacity: usize) -> i32; }
    let n = unsafe { xip_device_info(out.as_mut_ptr(), out.len()) };
    if n < 0 { Err(n) } else { Ok(n as usize) }
}

/// Publish a native host component's copied CBOR map; an empty slice clears it.
#[cfg(target_os = "espidf")]
pub fn info_publish(component: i32, method: i32, map: &[u8]) -> Result<(), i32> {
    extern "C" { fn xip_info_publish(component: i32, method: i32, data: *const u8, len: usize) -> i32; }
    let rc = unsafe { xip_info_publish(component, method, map.as_ptr(), map.len()) };
    if rc < 0 { Err(rc) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_device_a_call_fails() {
        let mut answer = [0u8; 8];
        assert_eq!(call("lora", b"idle", 43, 4, &[], &mut answer), Err(xip_abi::ERR_NOTFOUND));
        assert_eq!(status(0).running, false);
        assert!(find("lora").is_none());
        assert!(stop_all(10));
    }
}

/// Validated installed module API metadata.
pub mod module_api;
