//! ESP-IDF Wi-Fi initialization shared with applications using this host.
//! Radio modes, NAN/NOW callbacks and packet ownership remain with the application.

/// Build the driver's initialization configuration. The NVS memory-budget
/// overrides are shared with the C host and the standalone loader (`wl`).
/// The caller owns `esp_wifi_init` and its radio lifecycle.
pub fn init_config(sta_ampdu_enabled: bool) -> esp_idf_sys::wifi_init_config_t {
    let mut config = esp_idf_sys::wifi_init_config_t {
        osi_funcs: core::ptr::addr_of_mut!(esp_idf_sys::g_wifi_osi_funcs),
        wpa_crypto_funcs: unsafe { esp_idf_sys::g_wifi_default_wpa_crypto_funcs },
        static_rx_buf_num: esp_idf_sys::CONFIG_ESP_WIFI_STATIC_RX_BUFFER_NUM as i32,
        dynamic_rx_buf_num: esp_idf_sys::CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM as i32,
        tx_buf_type: esp_idf_sys::CONFIG_ESP_WIFI_TX_BUFFER_TYPE as i32,
        static_tx_buf_num: esp_idf_sys::WIFI_STATIC_TX_BUFFER_NUM as i32,
        dynamic_tx_buf_num: esp_idf_sys::WIFI_DYNAMIC_TX_BUFFER_NUM as i32,
        rx_mgmt_buf_type: esp_idf_sys::CONFIG_ESP_WIFI_DYNAMIC_RX_MGMT_BUF as i32,
        rx_mgmt_buf_num: esp_idf_sys::WIFI_RX_MGMT_BUF_NUM_DEF as i32,
        cache_tx_buf_num: esp_idf_sys::WIFI_CACHE_TX_BUFFER_NUM as i32,
        csi_enable: esp_idf_sys::WIFI_CSI_ENABLED as i32,
        ampdu_rx_enable: if sta_ampdu_enabled { esp_idf_sys::WIFI_AMPDU_RX_ENABLED as i32 } else { 0 },
        ampdu_tx_enable: if sta_ampdu_enabled { esp_idf_sys::WIFI_AMPDU_TX_ENABLED as i32 } else { 0 },
        amsdu_tx_enable: esp_idf_sys::WIFI_AMSDU_TX_ENABLED as i32,
        // An application's transient STA profile must not reopen stale IDF Wi-Fi NVS.
        nvs_enable: 0,
        nano_enable: esp_idf_sys::WIFI_NANO_FORMAT_ENABLED as i32,
        rx_ba_win: if sta_ampdu_enabled { esp_idf_sys::WIFI_DEFAULT_RX_BA_WIN as i32 } else { 0 },
        wifi_task_core_id: esp_idf_sys::WIFI_TASK_CORE_ID as i32,
        beacon_max_len: esp_idf_sys::WIFI_SOFTAP_BEACON_MAX_LEN as i32,
        mgmt_sbuf_num: esp_idf_sys::WIFI_MGMT_SBUF_NUM as i32,
        feature_caps: esp_idf_sys::WIFI_FEATURE_CAPS as u64,
        sta_disconnected_pm: esp_idf_sys::WIFI_STA_DISCONNECTED_PM_ENABLED != 0,
        espnow_max_encrypt_num: esp_idf_sys::CONFIG_ESP_WIFI_ESPNOW_MAX_ENCRYPT_NUM as i32,
        tx_hetb_queue_num: esp_idf_sys::WIFI_TX_HETB_QUEUE_NUM as i32,
        dump_hesigb_enable: esp_idf_sys::WIFI_DUMP_HESIGB_ENABLED != 0,
        magic: esp_idf_sys::WIFI_INIT_CONFIG_MAGIC as i32,
    };
    if let Ok(nvs) = crate::settings::Namespace::open(c"wl", false) {
        let setting = |key, default: i32, lo, hi| i32::from(nvs.get_u16(key).unwrap_or(default as u16)).clamp(lo, hi);
        config.static_rx_buf_num = setting(c"rx_static", config.static_rx_buf_num, 2, 64);
        config.dynamic_rx_buf_num = setting(c"rx_dynamic", config.dynamic_rx_buf_num, config.static_rx_buf_num, 128);
        if config.tx_buf_type == 1 { config.dynamic_tx_buf_num = setting(c"tx_dynamic", config.dynamic_tx_buf_num, 4, 128); }
        if config.ampdu_rx_enable != 0 {
            let max_ba = (2 * config.static_rx_buf_num).min(config.dynamic_rx_buf_num - 1);
            config.rx_ba_win = setting(c"rx_ba", config.rx_ba_win, 2, 64).min(max_ba);
        }
    }
    config
}

use core::{ffi::{c_char, c_void}, sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering}};

static STARTED: AtomicBool = AtomicBool::new(false);
static CONNECTED: AtomicBool = AtomicBool::new(false);
static SCAN_ONLY: AtomicBool = AtomicBool::new(false);
// 0 idle, 1 running, 2 completed by the ESP-IDF event task.
static SCAN_STATE: AtomicU8 = AtomicU8::new(0);
static NETIF: AtomicPtr<esp_idf_sys::esp_netif_t> = AtomicPtr::new(core::ptr::null_mut());
static mut ACTIVE_SSID: [u8; 33] = [0; 33];

unsafe extern "C" fn event_handler(_: *mut c_void, base: esp_idf_sys::esp_event_base_t,
                                    id: i32, _: *mut c_void) {
    if base == esp_idf_sys::WIFI_EVENT &&
        id == esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32 {
        SCAN_STATE.store(2, Ordering::Release);
    } else if base == esp_idf_sys::WIFI_EVENT &&
        id == esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32 {
        CONNECTED.store(false, Ordering::Release);
        if STARTED.load(Ordering::Acquire) { unsafe { esp_idf_sys::esp_wifi_connect() }; }
    } else if base == esp_idf_sys::IP_EVENT &&
        id == esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32 {
        CONNECTED.store(true, Ordering::Release);
    }
}

fn write_ipv4(dst: &mut [u8; 16], addr: u32) {
    let octets = addr.to_le_bytes();
    let mut pos = 0;
    for (i, octet) in octets.iter().enumerate() {
        if i != 0 { dst[pos] = b'.'; pos += 1; }
        let n = *octet;
        if n >= 100 { dst[pos] = b'0' + n / 100; pos += 1; }
        if n >= 10 { dst[pos] = b'0' + (n / 10) % 10; pos += 1; }
        dst[pos] = b'0' + n % 10; pos += 1;
    }
}

/// Reconcile the simple host's STA owner on its serialized worker. DMesh uses
/// its own owner and this initialization builder, with NAN/NOW unchanged.
pub(crate) fn start(profile: &crate::profile::TransportProfile) -> i32 {
    if STARTED.load(Ordering::Acquire) || SCAN_ONLY.load(Ordering::Acquire) { return -5; }
    let name = profile.ssid;
    let secret = profile.sta_passphrase;
    if name[0] == 0 { return -1; }
    unsafe {
        let rc = esp_idf_sys::esp_netif_init();
        if rc != 0 && rc != esp_idf_sys::ESP_ERR_INVALID_STATE { return -3; }
        let rc = esp_idf_sys::esp_event_loop_create_default();
        if rc != 0 && rc != esp_idf_sys::ESP_ERR_INVALID_STATE { return -3; }
        let netif = esp_idf_sys::esp_netif_create_default_wifi_sta();
        if netif.is_null() { return -6; }
        let mut init = init_config(profile.sta_ampdu_enabled);
        let mut rc = esp_idf_sys::esp_wifi_init(&mut init);
        if rc != 0 { esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast()); return -3; }
        rc = esp_idf_sys::esp_wifi_set_storage(esp_idf_sys::wifi_storage_t_WIFI_STORAGE_RAM);
        if rc != 0 { esp_idf_sys::esp_wifi_deinit(); esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast()); return -3; }
        let wifi_event = esp_idf_sys::esp_event_handler_register(
            esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32,
            Some(event_handler), core::ptr::null_mut());
        let ip_event = esp_idf_sys::esp_event_handler_register(
            esp_idf_sys::IP_EVENT, esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32,
            Some(event_handler), core::ptr::null_mut());
        let scan_event = esp_idf_sys::esp_event_handler_register(
            esp_idf_sys::WIFI_EVENT, esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32,
            Some(event_handler), core::ptr::null_mut());
        if wifi_event == 0 && ip_event == 0 && scan_event == 0 {
            let mut config = esp_idf_sys::wifi_config_t::default();
            config.sta.ssid[..32].copy_from_slice(&name[..32]);
            config.sta.password[..64].copy_from_slice(&secret[..64]);
            config.sta.channel = profile.sta_channel;
            config.sta.bssid_set = profile.sta_bssid_set;
            config.sta.bssid = profile.sta_bssid;
            rc = esp_idf_sys::esp_wifi_set_mode(esp_idf_sys::wifi_mode_t_WIFI_MODE_STA);
            if rc == 0 { rc = esp_idf_sys::esp_wifi_set_config(esp_idf_sys::wifi_interface_t_WIFI_IF_STA, &mut config); }
            if rc == 0 { rc = esp_idf_sys::esp_wifi_start(); }
            if rc == 0 {
                // Match the active STA startup used by DMesh. Its scheduling
                // owner may select another power-save mode after startup.
                rc = esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE);
                if rc != 0 { esp_idf_sys::esp_wifi_stop(); }
            }
            if rc == 0 {
                ACTIVE_SSID = name;
                NETIF.store(netif, Ordering::Release);
                CONNECTED.store(false, Ordering::Release);
                STARTED.store(true, Ordering::Release);
                if esp_idf_sys::esp_wifi_connect() == 0 { return 0; }
                STARTED.store(false, Ordering::Release);
                esp_idf_sys::esp_wifi_stop();
            }
        }
        if ip_event == 0 { esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::IP_EVENT,
            esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32, Some(event_handler)); }
        if scan_event == 0 { esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32, Some(event_handler)); }
        if wifi_event == 0 { esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32, Some(event_handler)); }
        esp_idf_sys::esp_wifi_deinit();
        esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast());
        NETIF.store(core::ptr::null_mut(), Ordering::Release);
    }
    -3
}

pub(crate) fn stop() {
    if SCAN_ONLY.swap(false, Ordering::AcqRel) {
        SCAN_STATE.store(0, Ordering::Release);
        unsafe {
            esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::WIFI_EVENT,
                esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32, Some(event_handler));
            esp_idf_sys::esp_wifi_stop();
            esp_idf_sys::esp_wifi_deinit();
        }
        return;
    }
    if !STARTED.swap(false, Ordering::AcqRel) { return; }
    CONNECTED.store(false, Ordering::Release);
    SCAN_STATE.store(0, Ordering::Release);
    unsafe {
        esp_idf_sys::esp_wifi_disconnect();
        esp_idf_sys::esp_wifi_stop();
        esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32, Some(event_handler));
        esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::IP_EVENT,
            esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32, Some(event_handler));
        esp_idf_sys::esp_event_handler_unregister(esp_idf_sys::WIFI_EVENT,
            esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32, Some(event_handler));
        esp_idf_sys::esp_wifi_deinit();
        let netif = NETIF.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !netif.is_null() { esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast()); }
        ACTIVE_SSID = [0; 33];
    }
}

pub(crate) unsafe extern "C" fn abi_status(state: *mut i32, ssid: *mut c_char,
                                             ip: *mut c_char, rssi: *mut i32) -> i32 {
    if state.is_null() || ssid.is_null() || ip.is_null() || rssi.is_null() { return -1; }
    let connected = CONNECTED.load(Ordering::Acquire);
    unsafe {
        *state = if !STARTED.load(Ordering::Acquire) { 0 } else if connected { 2 } else { 1 };
        core::ptr::copy_nonoverlapping(core::ptr::addr_of!(ACTIVE_SSID).cast::<u8>(), ssid.cast(), 33);
        core::ptr::write_bytes(ip, 0, 16);
        *rssi = 0;
        if connected {
            let mut ap = esp_idf_sys::wifi_ap_record_t::default();
            if esp_idf_sys::esp_wifi_sta_get_ap_info(&mut ap) == 0 { *rssi = ap.rssi as i32; }
            let netif = NETIF.load(Ordering::Acquire);
            if !netif.is_null() {
                let mut info = esp_idf_sys::esp_netif_ip_info_t::default();
                if esp_idf_sys::esp_netif_get_ip_info(netif, &mut info) == 0 {
                    let mut address = [0u8; 16];
                    write_ipv4(&mut address, info.ip.addr);
                    core::ptr::copy_nonoverlapping(address.as_ptr(), ip.cast(), 16);
                }
            }
        }
    }
    0
}

#[repr(C)]
struct Ap {
    ssid: [u8; 32], ssid_len: u8, bssid: [u8; 6], rssi: i8, channel: u8, auth: u8,
}
pub(crate) unsafe extern "C" fn abi_scan(out: *mut u8, max: usize) -> i32 {
    if max != 0 && out.is_null() { return -1; }
    if STARTED.load(Ordering::Acquire) && !CONNECTED.load(Ordering::Acquire) { return -5; }
    if !STARTED.load(Ordering::Acquire) && !SCAN_ONLY.load(Ordering::Acquire) {
        unsafe {
            let netif = esp_idf_sys::esp_netif_init();
            if netif != 0 && netif != esp_idf_sys::ESP_ERR_INVALID_STATE { return -3; }
            let event = esp_idf_sys::esp_event_loop_create_default();
            if event != 0 && event != esp_idf_sys::ESP_ERR_INVALID_STATE { return -3; }
            let mut init = init_config(true);
            if esp_idf_sys::esp_wifi_init(&mut init) != 0 { return -3; }
            if esp_idf_sys::esp_wifi_set_storage(esp_idf_sys::wifi_storage_t_WIFI_STORAGE_RAM) != 0 ||
               esp_idf_sys::esp_wifi_set_mode(esp_idf_sys::wifi_mode_t_WIFI_MODE_STA) != 0 ||
               esp_idf_sys::esp_wifi_start() != 0 {
                esp_idf_sys::esp_wifi_deinit(); return -3;
            }
            if esp_idf_sys::esp_event_handler_register(esp_idf_sys::WIFI_EVENT,
                esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32,
                Some(event_handler), core::ptr::null_mut()) != 0 {
                esp_idf_sys::esp_wifi_stop(); esp_idf_sys::esp_wifi_deinit(); return -3;
            }
            SCAN_ONLY.store(true, Ordering::Release);
        }
    }
    match SCAN_STATE.load(Ordering::Acquire) {
        0 => {
            SCAN_STATE.store(1, Ordering::Release);
            if unsafe { esp_idf_sys::esp_wifi_scan_start(core::ptr::null(), false) } != 0 {
                SCAN_STATE.store(0, Ordering::Release);
                if SCAN_ONLY.load(Ordering::Acquire) { stop(); }
                return -3;
            }
            return -5;
        }
        1 => return -5,
        _ => {}
    }
    SCAN_STATE.store(0, Ordering::Release);
    let mut records: [esp_idf_sys::wifi_ap_record_t; 16] = unsafe { core::mem::zeroed() };
    let mut count = max.min(records.len()) as u16;
    unsafe {
        if esp_idf_sys::esp_wifi_scan_get_ap_records(&mut count, records.as_mut_ptr()) != 0 {
            if SCAN_ONLY.load(Ordering::Acquire) { stop(); }
            return -3;
        }
        for (i, record) in records[..count as usize].iter().enumerate() {
            let len = record.ssid.iter().position(|b| *b == 0).unwrap_or(32);
            let mut ap = Ap { ssid: [0; 32], ssid_len: len as u8, bssid: record.bssid,
                              rssi: record.rssi, channel: record.primary, auth: record.authmode as u8 };
            ap.ssid[..len].copy_from_slice(&record.ssid[..len]);
            out.cast::<Ap>().add(i).write(ap);
        }
    }
    if SCAN_ONLY.load(Ordering::Acquire) { stop(); }
    count as i32
}
