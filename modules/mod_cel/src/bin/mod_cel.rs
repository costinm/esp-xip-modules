#![no_std]
#![no_main]
mod_cel::export!();
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! { xip_abi::heap::module_panic(info) }
