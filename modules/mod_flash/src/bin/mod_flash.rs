#![no_std]
#![no_main]
xip_abi::export_module!(esp_mod_flash::State, state = 128);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
