//! The Rust twin of `main/main.c`: start the host and the loader over a slot table and start the installed modules that ask
//! to run at boot (a console module, if installed). Everything else is in the C components and in `espmod-host`.
use espmod_host::{init, Features};
use espmod_host::MODULE_SLOTS;


fn main() {
    app_main();
}

extern "C" {
    fn espmod_log_to_rom();
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
    unsafe { espmod_log_to_rom() };
    mark(b"espmod-main: entry\n\0");
    esp_idf_sys::link_patches();
    unsafe {
        let mut err = esp_idf_sys::nvs_flash_init();
        if err == esp_idf_sys::ESP_ERR_NVS_NO_FREE_PAGES as i32 || err == esp_idf_sys::ESP_ERR_NVS_NEW_VERSION_FOUND as i32 {
            esp_idf_sys::nvs_flash_erase();
            err = esp_idf_sys::nvs_flash_init();
        }
        assert_eq!(err, 0, "nvs");
    }
    mark(b"espmod-main: nvs ok\n\0");
    // the host keeps the last events modules emit; this application needs no handler
    assert_eq!(init(&MODULE_SLOTS, "data\0", Features { modules: true, no_autostart: false, nv: None, pool: None, wifi: true }, None), 0, "host and loader");
    mark(b"espmod-main: running\n\0");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
