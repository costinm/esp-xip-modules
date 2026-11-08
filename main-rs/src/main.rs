//! The Rust main, twin of the C `main/main.c`: start the host and the loader over the module partition (it scans for modules) and, last, start the installed modules that ask
//! to run at boot (a debug module, if installed). Everything else is in the C components and in `xip-runtime-esp`.
use xip_runtime_esp::{autostart, init, Features};

fn main() {
    app_main();
}

extern "C" {
    fn xip_log_to_rom();
    fn xip_wifi_boot_network();
    fn xip_ble_install() -> i32;
    fn xip_ble_configure_boot(enabled: bool);
    fn xip_quic_classifier_enable(enabled: bool);
    fn xip_wifi_nv(key: *const core::ffi::c_char, key_len: usize,
        value: *const u8, value_len: usize, out: *mut u8, capacity: usize) -> i32;
    fn xip_heap_checkpoint(phase: *const core::ffi::c_char);
}

/// A line on the ROM console (before and independent of ESP-IDF logging).
fn mark(text: &'static [u8]) {
    unsafe { esp_idf_sys::esp_rom_printf(text.as_ptr().cast()) };
}

/// Our own entry point, as DMesh's main does: with esp-idf-sys's `binstart` ESP-IDF 6.0 reaches a lock of the C library's
/// standard streams that is not initialised yet (a load fault in `app_main`, measured on an ESP32-C6). Rust's `main` is not run.
#[no_mangle]
pub extern "C" fn app_main() {
    // ESP_LOG lines would be lost: with our own app_main the C library has no stdout
    unsafe { xip_heap_checkpoint(c"app-entry".as_ptr()); xip_log_to_rom() };
    esp_idf_sys::link_patches();

    unsafe {
        let mut err = esp_idf_sys::nvs_flash_init();
        if err == esp_idf_sys::ESP_ERR_NVS_NO_FREE_PAGES as i32 || err == esp_idf_sys::ESP_ERR_NVS_NEW_VERSION_FOUND as i32 {
            assert_eq!(esp_idf_sys::nvs_flash_erase(), 0, "erase exhausted NVS");
            err = esp_idf_sys::nvs_flash_init();
        }
        assert_eq!(err, 0, "nvs");
    }
    assert_eq!(init("data\0", Features { modules: true, ..Features::default() }, None), 0, "host and loader");
    unsafe {
        let _ = xip_ble_install();
        xip_quic_classifier_enable(true);
        let mut boot = [0u8; 4];
        let enabled = xip_wifi_nv(c"ble.boot".as_ptr(), 8, core::ptr::null(), 0,
            boot.as_mut_ptr(), boot.len()) > 0 && boot[0] == b'1';
        xip_ble_configure_boot(enabled);
        xip_heap_checkpoint(c"pre-network".as_ptr());
        xip_wifi_boot_network();
    }
    // The installed manifests decide what runs at boot, as in C Main.
    autostart();
    mark(b"main: ready\n\0");
}
