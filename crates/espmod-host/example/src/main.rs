//! The Rust twin of `main/main.c`: start the host and the loader over a slot table and take commands on the serial
//! port. Everything else is in the C components (`espmod_host`, `espmod_loader`, `espmod_install`) and in `modules/mod_console` and in `espmod-host`.
use espmod_host::{console, init, slot, Features, Slot};

/// Every module of this repository, flashed into its slot (tools/mkmod.py reads this table with `--slots`). Until the
/// loader records which module lives where, a new module means a new line here.
static SLOTS: [Slot; 4] = [
    slot!("hello", 1, 0x00000, 0x10000),
    slot!("test", 2, 0x10000, 0x10000),
    slot!("lora", 43, 0x20000, 0x10000),
    slot!("hw", 45, 0x30000, 0x10000),
];

fn main() {
    esp_idf_sys::link_patches();
    unsafe {
        let mut err = esp_idf_sys::nvs_flash_init();
        if err == esp_idf_sys::ESP_ERR_NVS_NO_FREE_PAGES as i32 || err == esp_idf_sys::ESP_ERR_NVS_NEW_VERSION_FOUND as i32 {
            esp_idf_sys::nvs_flash_erase();
            err = esp_idf_sys::nvs_flash_init();
        }
        assert_eq!(err, 0, "nvs");
    }
    // the host keeps the last events modules emit for the console's `events`; this application needs no handler
    assert_eq!(init(&SLOTS, "data\0", Features { modules: true, network: true }, None), 0, "host and loader");
    assert_eq!(console::start(), 0, "console");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
