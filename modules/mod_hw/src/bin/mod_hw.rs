#![no_std]
#![no_main]

use core::panic::PanicInfo;

esp_mod_hw::export!();

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }
