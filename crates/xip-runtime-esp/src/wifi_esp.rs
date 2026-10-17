//! ESP-IDF Wi-Fi initialization shared with applications using this host.
//! Radio modes, NAN/NOW callbacks and packet ownership remain with the application.

/// Build the driver's initialization configuration. The NVS memory-budget
/// overrides are shared with the C host and the standalone loader (`wl`).
/// The caller owns `esp_wifi_init` and its radio lifecycle.
pub fn init_config(sta_ampdu_enabled: bool) -> esp_idf_sys::wifi_init_config_t {
    init_config_with(sta_ampdu_enabled, [0; 4])
}

/// [`init_config`] with the buffer sizes of a running application, `[rx_static, rx_dynamic, tx_dynamic, rx_ba]`, each over the
/// NVS default when not 0 (the `wifi-tuning` component).
pub fn init_config_with(sta_ampdu_enabled: bool, buffers: [u16; 4]) -> esp_idf_sys::wifi_init_config_t {
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
    // The wifi-tuning component over the NVS defaults.
    let [rx_static, rx_dynamic, tx_dynamic, rx_ba] = buffers.map(i32::from);
    if rx_static != 0 { config.static_rx_buf_num = rx_static; }
    if rx_dynamic != 0 { config.dynamic_rx_buf_num = rx_dynamic; }
    config.dynamic_rx_buf_num = config.dynamic_rx_buf_num.max(config.static_rx_buf_num);
    if tx_dynamic != 0 && config.tx_buf_type == 1 { config.dynamic_tx_buf_num = tx_dynamic; }
    if config.ampdu_rx_enable != 0 {
        if rx_ba != 0 { config.rx_ba_win = rx_ba; }
        config.rx_ba_win = config.rx_ba_win.min((2 * config.static_rx_buf_num).min(config.dynamic_rx_buf_num - 1));
    }
    config
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The radio bring-up. This follows DMesh's `wifi_esp.rs` (`init_sta`, `init_nan_now`, `start_sta_driver_once`, `stop_sta`) step by
// step, in the same order and with the same driver settings, because that sequence went through a lot of testing on the boards.
// What DMesh decides by policy (scan-then-choose of the access point, the reconnect task, its fixed AP name, NAN discovery
// windows, its diagnostics log) is not here: the wanted state comes from the `wifi` intent, a failed association is retried, and
// the ESP-NOW link (`espnow_esp`) is started with the radio. The C twin is `components/xip_runtime/src/xip_wifi.c`.
// ---------------------------------------------------------------------------------------------------------------------------------
use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, Ordering},
};

/// 802.11 b/g/n, as DMesh sets on the STA. ESP-IDF supports no n-only 2.4 GHz bitmap; n lets the association negotiate HT20 and AMPDU.
const STA_PROTOCOL: u16 = (esp_idf_sys::WIFI_PROTOCOL_11B | esp_idf_sys::WIFI_PROTOCOL_11G | esp_idf_sys::WIFI_PROTOCOL_11N) as u16;
/// HT20: the data lane must coexist with NOW on 2.4 GHz, where an HT40 secondary channel is less robust.
const STA_HT40: bool = false;
/// AP beacons are the timing fallback of the soft cluster: 500 TU is about 512 ms and a supported SoftAP interval.
const AP_BEACON_TU: u16 = 500;
/// The channel of an unassociated radio (ESP-NOW only, or AP) when the intent names none: DMesh's out-of-box NOW channel.
const DEFAULT_CHANNEL: u8 = 6;
/// `WIFI_REASON_STA_LEAVING`: after our own `esp_wifi_disconnect`; not an association failure.
const WIFI_REASON_STA_LEAVING: u8 = 36;

extern "C" {
    fn nvs_flash_init() -> i32;
    fn esp_wifi_config_11b_rate(interface: esp_idf_sys::wifi_interface_t, disable: bool) -> i32;
    // Private libpp receive-filter hooks (the same declarations as DMesh): the STA lane is 0.
    fn ic_rx_disable_bssid_check(interface_id: u8);
    fn ic_rx_enable_bssid_check(interface_id: u8);
    fn ic_set_rx_policy_ubssid_check(interface_id: u8, enabled: bool) -> bool;
}

static STARTED: AtomicBool = AtomicBool::new(false); // the radio is up for the wanted state (`start` succeeded)
static CONNECTED: AtomicBool = AtomicBool::new(false); // the station has an address
static ASSOCIATED: AtomicBool = AtomicBool::new(false); // the station is associated (the driver's transition is the authority)
static OWNER_DISCONNECT_PENDING: AtomicBool = AtomicBool::new(false);
static WANT_STA: AtomicBool = AtomicBool::new(false);
static AP_ENABLED: AtomicBool = AtomicBool::new(false);
static SCAN_ONLY: AtomicBool = AtomicBool::new(false);
// 0 idle, 1 running, 2 completed by the ESP-IDF event task.
static SCAN_STATE: AtomicU8 = AtomicU8::new(0);
static STA_NETIF: AtomicPtr<esp_idf_sys::esp_netif_t> = AtomicPtr::new(core::ptr::null_mut());
static AP_NETIF: AtomicPtr<esp_idf_sys::esp_netif_t> = AtomicPtr::new(core::ptr::null_mut());
static mut ACTIVE_SSID: [u8; 33] = [0; 33];
// The driver epoch: it stays initialized across radio epochs (a stop keeps it); a changed pre-start policy (AMPDU, 11b rates)
// is only consumed by a full re-initialization.
static DRIVER_INITIALIZED: AtomicBool = AtomicBool::new(false);
static DRIVER_RUNNING: AtomicBool = AtomicBool::new(false);
static DRIVER_AMPDU: AtomicBool = AtomicBool::new(true);
static DRIVER_11B_DISABLED: AtomicBool = AtomicBool::new(true);
static EVENT_HANDLERS_REGISTERED: AtomicBool = AtomicBool::new(false);
static PHY_NVS_INITIALIZED: AtomicBool = AtomicBool::new(false);
// ESP-IDF's `esp_wifi_get_channel` can return an error while an unassociated STA has already accepted `esp_wifi_set_channel`:
// keep only a channel this owner applied, so connectionless NOW transmit and the idle receiver use the same one.
static APPLIED_CHANNEL: AtomicU8 = AtomicU8::new(0);
static CONNECT_STARTED_MS: AtomicU32 = AtomicU32::new(0);

/// The radio interfaces of the owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioInterface {
    Sta,
    Ap,
}
fn interface_native(interface: RadioInterface) -> esp_idf_sys::wifi_interface_t {
    match interface {
        RadioInterface::Sta => esp_idf_sys::wifi_interface_t_WIFI_IF_STA,
        RadioInterface::Ap => esp_idf_sys::wifi_interface_t_WIFI_IF_AP,
    }
}
pub fn interface_id(interface: RadioInterface) -> esp_idf_sys::wifi_interface_t {
    interface_native(interface)
}
pub fn interface_mac(interface: RadioInterface) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    (unsafe { esp_idf_sys::esp_wifi_get_mac(interface_native(interface), mac.as_mut_ptr()) } == esp_idf_sys::ESP_OK).then_some(mac)
}
pub fn sta_associated() -> bool {
    ASSOCIATED.load(Ordering::Acquire)
}
/// An AP-only personality: the AP is up and there is no associated station to reply from.
pub fn ap_only_active() -> bool {
    AP_ENABLED.load(Ordering::Acquire) && !sta_associated()
}

/// The channel and secondary channel the radio is on, as DMesh reports it for action transmit.
pub fn current_channel() -> Option<(u8, esp_idf_sys::wifi_second_chan_t)> {
    let applied = APPLIED_CHANNEL.load(Ordering::Acquire);
    // ESP-IDF reports its idle STA default (channel 1) while unassociated, even after the owner selected another one. For
    // unassociated action TX the selected channel is authoritative; an associated STA keeps the live driver query below.
    if !sta_associated() && (1..=13).contains(&applied) {
        return Some((applied, esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE));
    }
    let mut channel = 0u8;
    let mut secondary = esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE;
    if unsafe { esp_idf_sys::esp_wifi_get_channel(&mut channel, &mut secondary) } == esp_idf_sys::ESP_OK && (1..=13).contains(&channel) {
        APPLIED_CHANNEL.store(channel, Ordering::Release);
        return Some((channel, secondary));
    }
    if (1..=13).contains(&applied) { Some((applied, esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE)) } else { None }
}

extern "C" {
    // the early receive filter of the runtime (components/xip_runtime/src/xip_rx_filter.c)
    fn xip_rx_filter_local(sta: *const u8, ap: *const u8);
    fn xip_rx_filter_bssid(bssid: *const u8);
}

/// A scan-only epoch is running (the radio is up for nothing else).
pub(crate) fn scan_only() -> bool {
    SCAN_ONLY.load(Ordering::Acquire)
}

/// The radio is up for a wanted state (not only a scan).
pub(crate) fn radio_up() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Pin the radio to a 2.4 GHz primary channel with no secondary channel.
fn set_ht20_channel(channel: u8) -> bool {
    let channel = channel.clamp(1, 13);
    if unsafe { esp_idf_sys::esp_wifi_set_channel(channel, esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE) } != esp_idf_sys::ESP_OK {
        return false;
    }
    APPLIED_CHANNEL.store(channel, Ordering::Release);
    true
}

/// The private BSSID policy of one receive lane (DMesh's `set_bssid_check_disabled`): disabled, the radio hands up frames of
/// other BSSIDs, which unassociated ESP-NOW receive needs.
fn set_bssid_check_disabled(interface_id: u8, disabled: bool) -> bool {
    unsafe {
        if disabled {
            let updated = ic_set_rx_policy_ubssid_check(interface_id, false);
            ic_rx_disable_bssid_check(interface_id);
            updated
        } else {
            ic_rx_enable_bssid_check(interface_id);
            ic_set_rx_policy_ubssid_check(interface_id, true)
        }
    }
}

// PHY calibration requires an initialized NVS partition even though the Wi-Fi driver never loads or saves a persisted configuration.
fn initialize_phy_nvs() -> bool {
    if PHY_NVS_INITIALIZED.load(Ordering::Acquire) {
        return true;
    }
    let result = unsafe { nvs_flash_init() };
    if result == esp_idf_sys::ESP_OK || result == esp_idf_sys::ESP_ERR_INVALID_STATE {
        PHY_NVS_INITIALIZED.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn ensure_link_local(netif: *mut esp_idf_sys::esp_netif_t) {
    if !netif.is_null() {
        // ESP-IDF requires the netif to be up; the START events arrive after the interface has been brought up.
        let _ = unsafe { esp_idf_sys::esp_netif_create_ip6_linklocal(netif) };
    }
}

unsafe extern "C" fn event_handler(_: *mut c_void, base: esp_idf_sys::esp_event_base_t, id: i32, data: *mut c_void) {
    let wifi = base == unsafe { esp_idf_sys::WIFI_EVENT };
    if wifi && id == esp_idf_sys::wifi_event_t_WIFI_EVENT_SCAN_DONE as i32 {
        SCAN_STATE.store(2, Ordering::Release);
    } else if wifi && id == esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_START as i32 {
        ensure_link_local(STA_NETIF.load(Ordering::Acquire));
    } else if wifi && id == esp_idf_sys::wifi_event_t_WIFI_EVENT_AP_START as i32 {
        ensure_link_local(AP_NETIF.load(Ordering::Acquire));
    } else if wifi && id == esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_CONNECTED as i32 {
        ensure_link_local(STA_NETIF.load(Ordering::Acquire));
        ASSOCIATED.store(true, Ordering::Release);
        if !data.is_null() {
            unsafe { xip_rx_filter_bssid((*(data.cast::<esp_idf_sys::wifi_event_sta_connected_t>())).bssid.as_ptr()) };
        }
    } else if wifi && id == esp_idf_sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED as i32 {
        let reason = if data.is_null() { 0 } else { unsafe { (*(data.cast::<esp_idf_sys::wifi_event_sta_disconnected_t>())).reason } };
        let owner_leave = OWNER_DISCONNECT_PENDING.swap(false, Ordering::AcqRel);
        let intentional = reason == WIFI_REASON_STA_LEAVING || (owner_leave && matches!(reason, 3 | 8));
        ASSOCIATED.store(false, Ordering::Release);
        CONNECTED.store(false, Ordering::Release);
        unsafe { xip_rx_filter_bssid(core::ptr::null()) };
        // DMesh's Main decides when to look for the network again; the simple owner retries the configured one
        if !intentional && STARTED.load(Ordering::Acquire) && WANT_STA.load(Ordering::Acquire) {
            unsafe { esp_idf_sys::esp_wifi_connect() };
        }
    } else if !wifi && id == esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32 {
        CONNECTED.store(true, Ordering::Release);
    }
}

/// Subscribe once to the driver's events: a stop/start keeps the default event loop, so registering again would duplicate every transition.
unsafe fn register_event_handlers() -> bool {
    if EVENT_HANDLERS_REGISTERED.load(Ordering::Acquire) {
        return true;
    }
    let wifi = unsafe { esp_idf_sys::esp_event_handler_register(esp_idf_sys::WIFI_EVENT, esp_idf_sys::ESP_EVENT_ANY_ID, Some(event_handler), core::ptr::null_mut()) };
    let ip = unsafe {
        esp_idf_sys::esp_event_handler_register(
            esp_idf_sys::IP_EVENT,
            esp_idf_sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32,
            Some(event_handler),
            core::ptr::null_mut(),
        )
    };
    if wifi == esp_idf_sys::ESP_OK && ip == esp_idf_sys::ESP_OK {
        EVENT_HANDLERS_REGISTERED.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

/// The platform setup before `esp_wifi_init`, in DMesh's order: PHY NVS, netif, default event loop.
unsafe fn platform_init() -> bool {
    if !initialize_phy_nvs() {
        return false;
    }
    let netif = unsafe { esp_idf_sys::esp_netif_init() };
    if netif != esp_idf_sys::ESP_OK && netif != esp_idf_sys::ESP_ERR_INVALID_STATE {
        return false;
    }
    let event_loop = unsafe { esp_idf_sys::esp_event_loop_create_default() };
    event_loop == esp_idf_sys::ESP_OK || event_loop == esp_idf_sys::ESP_ERR_INVALID_STATE
}

/// A changed AMPDU or 11b policy is consumed only by re-initializing the driver (DMesh's `prepare_sta_driver_policy`).
fn driver_policy_changed(ampdu: bool, rates_11b_disabled: bool) -> bool {
    DRIVER_INITIALIZED.load(Ordering::Acquire)
        && (DRIVER_AMPDU.load(Ordering::Acquire) != ampdu || DRIVER_11B_DISABLED.load(Ordering::Acquire) != rates_11b_disabled)
}

/// `esp_wifi_init` once per driver epoch.
unsafe fn init_driver(ampdu: bool, rates_11b_disabled: bool, buffers: [u16; 4]) -> bool {
    if !DRIVER_INITIALIZED.swap(true, Ordering::AcqRel) {
        DRIVER_AMPDU.store(ampdu, Ordering::Release);
        DRIVER_11B_DISABLED.store(rates_11b_disabled, Ordering::Release);
        let mut init = init_config_with(ampdu, buffers);
        let result = unsafe { esp_idf_sys::esp_wifi_init(&mut init) };
        if result != esp_idf_sys::ESP_OK && result != esp_idf_sys::ESP_ERR_INVALID_STATE {
            DRIVER_INITIALIZED.store(false, Ordering::Release);
            return false;
        }
    }
    let _ = unsafe { esp_idf_sys::esp_wifi_set_storage(esp_idf_sys::wifi_storage_t_WIFI_STORAGE_RAM) };
    true
}

unsafe fn start_driver_once() -> i32 {
    if DRIVER_RUNNING.load(Ordering::Acquire) {
        return esp_idf_sys::ESP_OK;
    }
    let result = unsafe { esp_idf_sys::esp_wifi_start() };
    if result == esp_idf_sys::ESP_OK {
        DRIVER_RUNNING.store(true, Ordering::Release);
    }
    result
}

/// The soft AP, configured before the one Wi-Fi start of an AP+STA epoch (DMesh's `configure_unassociated_dmesh_ap`, with the name and
/// passphrase of the intent in place of its fixed ones).
unsafe fn configure_ap(intent: &xip_abi::WifiIntent, channel: u8) -> bool {
    if AP_NETIF.load(Ordering::Acquire).is_null() {
        let netif = unsafe { esp_idf_sys::esp_netif_create_default_wifi_ap() };
        if netif.is_null() {
            return false;
        }
        AP_NETIF.store(netif, Ordering::Release);
    }
    let mut ap = esp_idf_sys::wifi_ap_config_t::default();
    let open = intent.ap_psk_len == 0;
    if intent.ap_ssid_len != 0 {
        let n = intent.ap_ssid_len as usize;
        ap.ssid[..n].copy_from_slice(&intent.ap_ssid[..n]);
        ap.ssid_len = n as u8;
    } else if let Some(mac) = interface_mac(RadioInterface::Ap) {
        // the default name, as the C host: xip-<last two bytes of the AP MAC>
        let name = [b'x', b'i', b'p', b'-', HEX[(mac[4] >> 4) as usize], HEX[(mac[4] & 15) as usize], HEX[(mac[5] >> 4) as usize], HEX[(mac[5] & 15) as usize]];
        ap.ssid[..name.len()].copy_from_slice(&name);
        ap.ssid_len = name.len() as u8;
    }
    if !open {
        let n = intent.ap_psk_len as usize;
        ap.password[..n].copy_from_slice(&intent.ap_psk[..n]);
    }
    ap.channel = channel;
    ap.authmode = if open { esp_idf_sys::wifi_auth_mode_t_WIFI_AUTH_OPEN } else { esp_idf_sys::wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK };
    // Android's app-scoped WPA2 requests must not require PMF, but accepting it lets modern Android devices negotiate it
    ap.pmf_cfg.capable = !open;
    ap.pmf_cfg.required = false;
    ap.max_connection = 4;
    ap.beacon_interval = AP_BEACON_TU.clamp(100, 60_000);
    let mut config = esp_idf_sys::wifi_config_t { ap };
    unsafe { esp_idf_sys::esp_wifi_set_config(esp_idf_sys::wifi_interface_t_WIFI_IF_AP, &mut config) == esp_idf_sys::ESP_OK }
}
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Bring the radio up for the wanted state: the station (and its association), the soft AP, and ESP-NOW (always, with the radio).
/// DMesh's `init_sta` / `init_nan_now`, step by step. Returns 0 or a negative result of the host ABI.
pub(crate) fn start(intent: &xip_abi::WifiIntent, tuning: &xip_abi::WifiTuning) -> i32 {
    if STARTED.load(Ordering::Acquire) || SCAN_ONLY.load(Ordering::Acquire) {
        return -5;
    }
    let sta = intent.sta == 1;
    let ap = intent.ap == 1;
    // DMesh's defaults: AMPDU on, 11b rates off, BSSID check off (frames of other BSSIDs reach unassociated NOW receive)
    let ampdu = tuning.ampdu != xip_abi::TUNE_OFF;
    let rates_11b_disabled = tuning.rates_11b != xip_abi::TUNE_ON;
    let bssid_check_disabled = tuning.bssid_check != xip_abi::TUNE_ON;
    let channel = if intent.channel == 0 { DEFAULT_CHANNEL } else { intent.channel.clamp(1, 13) };
    unsafe {
        if driver_policy_changed(ampdu, rates_11b_disabled) {
            stop();
            restart_driver();
        }
        if !platform_init() {
            return -3;
        }
        // The standard ESP-IDF ordering: create the default STA glue after esp-netif and the event loop but before `esp_wifi_init`
        // (`esp_wifi_internal_tx` is the glue's egress API; creating it later accepted frames but left the Ethernet path undrained).
        if STA_NETIF.load(Ordering::Acquire).is_null() {
            let netif = esp_idf_sys::esp_netif_create_default_wifi_sta();
            if netif.is_null() {
                return -6;
            }
            STA_NETIF.store(netif, Ordering::Release);
        }
        // the default handlers bring the netif up before our callbacks create IPv6 link-local
        if !register_event_handlers() {
            return -3;
        }
        if !init_driver(ampdu, rates_11b_disabled, crate::wifi::buffers(tuning)) {
            return -3;
        }
        let mode = if ap { esp_idf_sys::wifi_mode_t_WIFI_MODE_APSTA } else { esp_idf_sys::wifi_mode_t_WIFI_MODE_STA };
        let mode_result = esp_idf_sys::esp_wifi_set_mode(mode);
        if mode_result != esp_idf_sys::ESP_OK && mode_result != esp_idf_sys::ESP_ERR_INVALID_STATE {
            return -3;
        }
        if ap && !configure_ap(intent, channel) {
            return -3;
        }
        if sta {
            let mut config = esp_idf_sys::wifi_sta_config_t::default();
            let n = intent.sta_ssid_len as usize;
            config.ssid[..n].copy_from_slice(&intent.sta_ssid[..n]);
            let open = intent.sta_psk_len == 0;
            if !open {
                let m = intent.sta_psk_len as usize;
                config.password[..m].copy_from_slice(&intent.sta_psk[..m]);
                // a configured WPA credential. DMesh sets the threshold to what its scan found (WPA2: WPA2_PSK, PMF capable); without the scan the
                // lowest it accepts is WPA2, so a WPA2-only AP, a WPA2/WPA3 transition AP and a WPA3 AP all join (a WPA2_WPA3 threshold rejects WPA2-only)
                config.threshold.authmode = esp_idf_sys::wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK;
                config.pmf_cfg.capable = true;
                config.pmf_cfg.required = false;
                config.sae_pwe_h2e = esp_idf_sys::wifi_sae_pwe_method_t_WPA3_SAE_PWE_BOTH;
            } else {
                config.threshold.authmode = esp_idf_sys::wifi_auth_mode_t_WIFI_AUTH_OPEN;
                config.pmf_cfg.capable = false;
                config.pmf_cfg.required = false;
            }
            config.channel = intent.channel;
            config.bssid_set = intent.bssid != [0; 6];
            config.bssid = intent.bssid;
            let mut config = esp_idf_sys::wifi_config_t { sta: config };
            if esp_idf_sys::esp_wifi_set_config(esp_idf_sys::wifi_interface_t_WIFI_IF_STA, &mut config) != esp_idf_sys::ESP_OK {
                return -3;
            }
        }
        let mut protocols = esp_idf_sys::wifi_protocols_t { ghz_2g: STA_PROTOCOL, ghz_5g: 0 };
        if esp_idf_sys::esp_wifi_set_protocols(esp_idf_sys::wifi_interface_t_WIFI_IF_STA, &mut protocols) != esp_idf_sys::ESP_OK {
            return -3;
        }
        let bandwidth = if STA_HT40 { esp_idf_sys::wifi_bandwidth_t_WIFI_BW40 } else { esp_idf_sys::wifi_bandwidth_t_WIFI_BW20 };
        if esp_idf_sys::esp_wifi_set_bandwidth(esp_idf_sys::wifi_interface_t_WIFI_IF_STA, bandwidth) != esp_idf_sys::ESP_OK {
            return -3;
        }
        // after init and before start; it includes the negotiated legacy/basic-rate set, so it is never applied to a running driver
        let legacy_rate = if DRIVER_RUNNING.load(Ordering::Acquire) {
            esp_idf_sys::ESP_OK
        } else {
            esp_wifi_config_11b_rate(esp_idf_sys::wifi_interface_t_WIFI_IF_STA, rates_11b_disabled)
        };
        if legacy_rate != esp_idf_sys::ESP_OK {
            return -3;
        }
        ASSOCIATED.store(false, Ordering::Release);
        CONNECTED.store(false, Ordering::Release);
        let started = start_driver_once();
        if started != esp_idf_sys::ESP_OK && started != esp_idf_sys::ESP_ERR_INVALID_STATE {
            return -3;
        }
        if !set_bssid_check_disabled(0, bssid_check_disabled) {
            return -3;
        }
        let _ = esp_idf_sys::esp_wifi_set_ps(esp_idf_sys::wifi_ps_type_t_WIFI_PS_NONE);
        WANT_STA.store(sta, Ordering::Release);
        AP_ENABLED.store(ap, Ordering::Release);
        if sta {
            ACTIVE_SSID = [0; 33];
            ACTIVE_SSID[..intent.sta_ssid_len as usize].copy_from_slice(&intent.sta_ssid[..intent.sta_ssid_len as usize]);
            CONNECT_STARTED_MS.store((esp_idf_sys::esp_timer_get_time().max(0) as u64 / 1_000) as u32, Ordering::Release);
            STARTED.store(true, Ordering::Release);
            if esp_idf_sys::esp_wifi_connect() != esp_idf_sys::ESP_OK {
                // the driver's own events report a failed attempt; the retry above covers it
            }
        } else if ap {
            // APSTA selects its configured channel as it starts: read the live value instead of calling `set_channel` after start,
            // which would add a driver transition to the out-of-box NOW test
            let mut primary = 0u8;
            let mut secondary = esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE;
            if esp_idf_sys::esp_wifi_get_channel(&mut primary, &mut secondary) != esp_idf_sys::ESP_OK || primary != channel {
                return -3;
            }
            APPLIED_CHANNEL.store(primary, Ordering::Release);
            STARTED.store(true, Ordering::Release);
        } else {
            // an unassociated ESP-NOW-only epoch stays unassociated and pins its channel
            let _ = esp_idf_sys::esp_wifi_disconnect();
            esp_idf_sys::vTaskDelay(5);
            if !set_ht20_channel(channel) {
                return -3;
            }
            STARTED.store(true, Ordering::Release);
        }
        // with the radio on, ESP-NOW is on (DMesh's `start_sta_extensions`, after the driver is live and the channel is pinned)
        {
            let sta = interface_mac(RadioInterface::Sta).unwrap_or([0; 6]);
            let ap = interface_mac(RadioInterface::Ap).unwrap_or([0; 6]);
            xip_rx_filter_local(sta.as_ptr(), if AP_ENABLED.load(Ordering::Acquire) { ap.as_ptr() } else { core::ptr::null() });
        }
        crate::espnow_esp::start();
    }
    0
}

/// Release the radio the way DMesh's `stop_sta` does: the NOW callback goes inert, the station leaves, the driver stops and the
/// STA netif is destroyed; the initialized driver is kept for the next epoch.
pub(crate) fn stop() {
    if SCAN_ONLY.swap(false, Ordering::AcqRel) {
        SCAN_STATE.store(0, Ordering::Release);
        DRIVER_RUNNING.store(false, Ordering::Release);
        unsafe { esp_idf_sys::esp_wifi_stop() };
        return;
    }
    if !STARTED.swap(false, Ordering::AcqRel) {
        return;
    }
    unsafe {
        xip_rx_filter_local(core::ptr::null(), core::ptr::null());
        xip_rx_filter_bssid(core::ptr::null());
    }
    crate::promisc_esp::stop();
    crate::espnow_esp::stop();
    DRIVER_RUNNING.store(false, Ordering::Release);
    SCAN_STATE.store(0, Ordering::Release);
    WANT_STA.store(false, Ordering::Release);
    AP_ENABLED.store(false, Ordering::Release);
    unsafe {
        OWNER_DISCONNECT_PENDING.store(ASSOCIATED.load(Ordering::Acquire), Ordering::Release);
        ASSOCIATED.store(false, Ordering::Release);
        CONNECTED.store(false, Ordering::Release);
        let _ = esp_idf_sys::esp_wifi_disconnect();
        let _ = esp_idf_sys::esp_wifi_stop();
        let netif = STA_NETIF.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !netif.is_null() {
            esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast());
        }
        ACTIVE_SSID = [0; 33];
    }
}

/// Recreate the driver so a changed `wifi_init_config_t` is consumed (stop/start alone leaves the AMPDU settings unchanged).
pub(crate) fn restart_driver() {
    unsafe {
        let result = esp_idf_sys::esp_wifi_deinit();
        let _ = result;
        DRIVER_INITIALIZED.store(false, Ordering::Release);
        DRIVER_RUNNING.store(false, Ordering::Release);
        let netif = STA_NETIF.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !netif.is_null() {
            esp_idf_sys::esp_netif_destroy_default_wifi(netif.cast());
        }
    }
}

pub(crate) unsafe extern "C" fn abi_state(state: *mut xip_abi::WifiState) -> i32 {
    if state.is_null() { return -1; }
    let st = unsafe { &mut *state };
    let size = st.size;
    *st = xip_abi::WifiState::default();
    st.size = size;
    if !STARTED.load(Ordering::Acquire) { return 0; }
    st.ap = u8::from(AP_ENABLED.load(Ordering::Acquire));
    if let Some((channel, _)) = current_channel() { st.channel = channel; }
    if !WANT_STA.load(Ordering::Acquire) { return 0; }
    let connected = CONNECTED.load(Ordering::Acquire);
    st.sta = if connected { 2 } else { 1 };
    if connected {
        let mut ap = esp_idf_sys::wifi_ap_record_t::default();
        if unsafe { esp_idf_sys::esp_wifi_sta_get_ap_info(&mut ap) } == 0 {
            st.rssi = ap.rssi; st.bssid = ap.bssid; st.channel = ap.primary;
        }
        let ssid = unsafe { core::ptr::addr_of!(ACTIVE_SSID).read() };
        let n = ssid.iter().position(|b| *b == 0).unwrap_or(32).min(32);
        st.ssid[..n].copy_from_slice(&ssid[..n]); st.ssid_len = n as u8;
        let netif = STA_NETIF.load(Ordering::Acquire);
        if !netif.is_null() {
            let mut info = esp_idf_sys::esp_netif_ip_info_t::default();
            if unsafe { esp_idf_sys::esp_netif_get_ip_info(netif, &mut info) } == 0 { st.ip4 = info.ip.addr.to_le_bytes(); }
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
    if !STARTED.load(Ordering::Acquire) && !SCAN_ONLY.load(Ordering::Acquire) {
        unsafe {
            if !platform_init() || !register_event_handlers() { return -3; }
            if !init_driver(true, true, [0; 4]) { return -3; }
            if esp_idf_sys::esp_wifi_set_mode(esp_idf_sys::wifi_mode_t_WIFI_MODE_STA) != 0 { return -3; }
            if start_driver_once() != 0 { return -3; }
            SCAN_ONLY.store(true, Ordering::Release);
        }
    }
    match SCAN_STATE.load(Ordering::Acquire) {
        0 => {
            SCAN_STATE.store(1, Ordering::Release);
            let rc = unsafe { esp_idf_sys::esp_wifi_scan_start(core::ptr::null(), false) };
            if rc != 0 {
                SCAN_STATE.store(0, Ordering::Release);
                if SCAN_ONLY.load(Ordering::Acquire) { stop(); }
                // busy joining: try again
                return if rc == esp_idf_sys::ESP_ERR_WIFI_STATE as i32 { -5 } else { -3 };
            }
            return -5;
        }
        1 => return -5,
        _ => {}
    }
    SCAN_STATE.store(0, Ordering::Release);
    // a scan leaves the channel: an unassociated radio goes back to the channel this owner pinned
    if STARTED.load(Ordering::Acquire) && !ASSOCIATED.load(Ordering::Acquire) {
        let ch = APPLIED_CHANNEL.load(Ordering::Acquire);
        if ch != 0 { unsafe { esp_idf_sys::esp_wifi_set_channel(ch, esp_idf_sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE) }; }
    }
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
