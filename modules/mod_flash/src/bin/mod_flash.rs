#![no_std]
#![no_main]
xip_abi::export_module!(esp_mod_flash::State, state = 128);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! { xip_abi::heap::module_panic(info) }
