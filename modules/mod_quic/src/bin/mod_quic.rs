#![no_std]
#![no_main]
use core::panic::PanicInfo;
esp_mod_quic::export!();
#[panic_handler]
fn panic(info: &PanicInfo)->!{ xip_abi::heap::module_panic(info) }
