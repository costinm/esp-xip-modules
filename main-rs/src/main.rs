//! The Rust main, twin of the C `main/main.c`: start the host and the loader over the module partition (it scans for modules) and, last, start the installed modules that ask
//! to run at boot (a console module, if installed). Everything else is in the C components and in `xip-runtime-esp`.
use xip_runtime_esp::{autostart, init, Features};


fn main() {
    app_main();
}

extern "C" {
    fn xip_log_to_rom();
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
    unsafe { xip_log_to_rom() };
    mark(b"xip-main: entry\n\0");
    esp_idf_sys::link_patches();
    unsafe {
        let mut err = esp_idf_sys::nvs_flash_init();
        if err == esp_idf_sys::ESP_ERR_NVS_NO_FREE_PAGES as i32 || err == esp_idf_sys::ESP_ERR_NVS_NEW_VERSION_FOUND as i32 {
            esp_idf_sys::nvs_flash_erase();
            err = esp_idf_sys::nvs_flash_init();
        }
        assert_eq!(err, 0, "nvs");
    }
    mark(b"xip-main: nvs ok\n\0");
    // the host keeps the last events modules emit; this application needs no handler
    assert_eq!(init("data\0", Features { modules: true, nv: None, pool: None, wifi: None }, None), 0, "host and loader");
    // last: what is installed and asks to run at boot (a console), now that the host and the loader are up
    autostart();
    mark(b"xip-main: running\n\0");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
