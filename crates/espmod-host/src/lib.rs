//! The Rust side of an esp-xip-modules application (the C side is `components/espmod_host`, `espmod_loader`): start the host
//! and the loader over a slot table, start, stop and call modules, receive their events.
//!
//! What is here is what two applications had in common, [DMesh](https://github.com/costinm/dmesh) and the example main in
//! `example/`. It moves bytes and knows nothing else: no module (its operations, its events and its field ids are the
//! module's own crate), no transport, no CBOR. A caller turns whatever it receives into
//! `(module, component, method, request bytes)` and gets the answer bytes back. The loader is only there on the device
//! (`target_os = "espidf"`); on a PC every call fails, so the callers can be tested with this crate as a stub.
//!
//! The ABI itself, for module code, is the `espmod-abi` crate.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(feature = "console")]
pub mod console;
mod loader;

pub use loader::{call, find, init, request_raw, slot_count, start, status, stop, stop_all, EventHandler, Features, Slot, SlotStatus};

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
