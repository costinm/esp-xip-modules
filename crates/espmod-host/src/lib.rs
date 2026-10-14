//! The Rust side of an esp-xip-modules application (the C side is `components/espmod_host`, `espmod_loader`): start the host
//! and the loader over a slot table, start, stop and call modules, receive their events.
//!
//! Generic host facilities include module lifecycle, packet-backed socket
//! readiness, settings, desired bearer profiles and ESP Wi-Fi initialization.
//! Products keep protocol parsing and radio scheduling policy in their own
//! owners. `profile` is a Rust API for those owners; the module interface is
//! generated separately as a C-compatible function table.
//!
//! The ABI itself, for module code, is the `espmod-abi` crate.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod loader;
pub mod profile;
#[cfg(target_os = "espidf")]
pub mod settings;

#[cfg(target_os = "espidf")]
pub mod lwip_io_esp;
#[cfg(target_os = "espidf")]
mod module_poll_esp;
#[cfg(target_os = "espidf")]
pub mod wifi_esp;
#[cfg(target_os = "espidf")]
mod wifi_hooks;

pub use loader::{call, find, init, manifest, request_raw, slot_count, start, status, stop, stop_all, EventHandler, Features, PoolOps, Slot, SlotStatus, MODULE_SLOTS};

/// Read host status without depending on a console module.
#[cfg(target_os = "espidf")]
pub fn device_info(out: &mut [u8]) -> Result<usize, i32> {
    extern "C" { fn espmod_device_info(out: *mut u8, capacity: usize) -> i32; }
    let n = unsafe { espmod_device_info(out.as_mut_ptr(), out.len()) };
    if n < 0 { Err(n) } else { Ok(n as usize) }
}

/// Publish a native host component's copied CBOR map; an empty slice clears it.
#[cfg(target_os = "espidf")]
pub fn info_publish(component: i32, method: i32, map: &[u8]) -> Result<(), i32> {
    extern "C" { fn espmod_info_publish(component: i32, method: i32, data: *const u8, len: usize) -> i32; }
    let rc = unsafe { espmod_info_publish(component, method, map.as_ptr(), map.len()) };
    if rc < 0 { Err(rc) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_macro_makes_a_nul_terminated_name() {
        static SLOTS: [Slot; 2] = [slot!("lora", 43, 0x00000, 0x10000), slot!("hw", 45, 0x10000, 0x10000)];
        let name = unsafe { core::slice::from_raw_parts(SLOTS[0].name_ptr(), 5) };
        assert_eq!(name, b"lora\0");
    }

    #[test]
    fn without_a_device_a_call_fails() {
        let mut answer = [0u8; 8];
        assert_eq!(call("lora", b"idle", 43, 4, &[], &mut answer), Err(espmod_abi::ERR_NOTFOUND));
        assert_eq!(status(0).running, false);
        assert!(find("lora").is_none());
        assert!(stop_all(10));
    }
}
