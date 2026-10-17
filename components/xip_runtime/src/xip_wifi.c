/* SPDX-License-Identifier: Apache-2.0 */
#include <stdio.h>
#include <string.h>
#include "sdkconfig.h"
#include "xip_wifi.h"
#include "xip_cbor.h"
#include "nvs.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "esp_wifi_types.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static uint16_t setting(nvs_handle_t nvs, const char *key, uint16_t def, uint16_t lo, uint16_t hi)
{
    uint16_t value = def;
    if (!nvs || nvs_get_u16(nvs, key, &value) != ESP_OK) value = def;
    return value < lo ? lo : value > hi ? hi : value;
}

void xip_wifi_apply_init_settings(wifi_init_config_t *cfg)
{
    nvs_handle_t nvs = 0;
    if (nvs_open("wl", NVS_READONLY, &nvs) != ESP_OK) nvs = 0;
    cfg->static_rx_buf_num = setting(nvs, "rx_static", cfg->static_rx_buf_num, 2, 64);
    cfg->dynamic_rx_buf_num = setting(nvs, "rx_dynamic", cfg->dynamic_rx_buf_num,
                                      cfg->static_rx_buf_num, 128);
    if (cfg->tx_buf_type == 1)
        cfg->dynamic_tx_buf_num = setting(nvs, "tx_dynamic", cfg->dynamic_tx_buf_num, 4, 128);
    if (cfg->ampdu_rx_enable) {
        int max_ba = 2 * cfg->static_rx_buf_num;
        if (max_ba >= cfg->dynamic_rx_buf_num) max_ba = cfg->dynamic_rx_buf_num - 1;
        cfg->rx_ba_win = setting(nvs, "rx_ba", cfg->rx_ba_win, 2, 64);
        if (cfg->rx_ba_win > max_ba) cfg->rx_ba_win = max_ba;
    }
    if (nvs) nvs_close(nvs);
}

int xip_wifi_nv(const char *key, size_t key_len, const uint8_t *value, size_t value_len,
                   uint8_t *out, size_t cap)
{
    static const struct { const char *name, *key, *ns; bool secret; } strings[] = {
        {"sta_ssid", "sta_ssid", "dmesh", false}, {"sec:sta", "sta", "sec", true},
    };
    if (!key || (!value && !out)) return -1;
    for (size_t i = 0; i < sizeof(strings)/sizeof(strings[0]); ++i) {
        if (strlen(strings[i].name) != key_len || memcmp(strings[i].name, key, key_len)) continue;
        if (strings[i].secret && !value) return -4;
        if (value_len > 63 || (value && memchr(value, 0, value_len))) return -1;
        char text[64] = {0}; if (value_len) memcpy(text, value, value_len);
        if (value && value_len) {
            bool valid = false;
            switch (i) {
            case 0: valid = value_len <= 32; break;
            case 1: valid = value_len >= 8; break;
            }
            if (!valid) return -1;
        }
        nvs_handle_t nvs;
        if (nvs_open(strings[i].ns, value ? NVS_READWRITE : NVS_READONLY, &nvs) != ESP_OK) return value ? -3 : -4;
        int result;
        if (value) {
            esp_err_t rc = value_len ? nvs_set_str(nvs, strings[i].key, text) : nvs_erase_key(nvs, strings[i].key);
            result = (rc == ESP_OK || rc == ESP_ERR_NVS_NOT_FOUND) && nvs_commit(nvs) == ESP_OK ? 0 : -3;
        } else {
            size_t n = sizeof(text);
            if (nvs_get_str(nvs, strings[i].key, text, &n) != ESP_OK) result = -4;
            else if (n-1 > cap) result = -5;
            else { memcpy(out, text, n-1); result = n-1; }
        }
        nvs_close(nvs); return result;
    }
    static const struct { const char *name, *nvs_key; uint16_t lo, hi; } keys[] = {
        {"wifi.rx_static", "rx_static", 2, 64}, {"wifi.rx_dynamic", "rx_dynamic", 2, 128},
        {"wifi.tx_dynamic", "tx_dynamic", 4, 128}, {"wifi.rx_ba", "rx_ba", 2, 64},
    };
    if (!key || (!value && !out)) return -1;
    for (size_t i = 0; i < sizeof(keys) / sizeof(keys[0]); ++i) {
        if (strlen(keys[i].name) != key_len || memcmp(key, keys[i].name, key_len)) continue;
        nvs_handle_t nvs;
        if (nvs_open("wl", value ? NVS_READWRITE : NVS_READONLY, &nvs) != ESP_OK)
            return value ? -3 : -4;
        int result;
        if (!value) {
            uint16_t number;
            if (nvs_get_u16(nvs, keys[i].nvs_key, &number) != ESP_OK) result = -4;
            else {
                int written = snprintf((char *)out, cap, "%u", number);
                result = written < 0 ? -3 : (size_t)written >= cap ? -5 : written;
            }
        } else if (!value_len) {
            esp_err_t rc = nvs_erase_key(nvs, keys[i].nvs_key);
            result = (rc == ESP_OK || rc == ESP_ERR_NVS_NOT_FOUND) && nvs_commit(nvs) == ESP_OK ? 0 : -3;
        } else {
            unsigned number = 0;
            bool valid = value_len <= 4;
            for (size_t j = 0; valid && j < value_len; ++j) {
                valid = value[j] >= '0' && value[j] <= '9';
                number = number * 10 + value[j] - '0';
            }
            if (!valid || number < keys[i].lo || number > keys[i].hi) result = -1;
            else result = nvs_set_u16(nvs, keys[i].nvs_key, number) == ESP_OK &&
                          nvs_commit(nvs) == ESP_OK ? 0 : -3;
        }
        nvs_close(nvs);
        return result;
    }
    return -4;
}

wifi_init_config_t xip_wifi_init_config(bool ampdu_enabled)
{
    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    cfg.nvs_enable = 0;
    if (!ampdu_enabled) {
        cfg.ampdu_rx_enable = 0;
        cfg.ampdu_tx_enable = 0;
        cfg.rx_ba_win = 0;
    }
    xip_wifi_apply_init_settings(&cfg);
    return cfg;
}

/* The wifi-tuning component over the NVS defaults: what the driver is initialized with. */
static wifi_init_config_t init_config(const xip_wifi_tuning *t)
{
    wifi_init_config_t cfg = xip_wifi_init_config(t->ampdu != XIP_TUNE_OFF);
    if (t->rx_static) cfg.static_rx_buf_num = t->rx_static;
    if (t->rx_dynamic) cfg.dynamic_rx_buf_num = t->rx_dynamic;
    if (cfg.dynamic_rx_buf_num < cfg.static_rx_buf_num) cfg.dynamic_rx_buf_num = cfg.static_rx_buf_num;
    if (t->tx_dynamic && cfg.tx_buf_type == 1) cfg.dynamic_tx_buf_num = t->tx_dynamic;
    if (cfg.ampdu_rx_enable) {
        int max_ba = 2 * cfg.static_rx_buf_num;
        if (max_ba >= cfg.dynamic_rx_buf_num) max_ba = cfg.dynamic_rx_buf_num - 1;
        if (t->rx_ba) cfg.rx_ba_win = t->rx_ba;
        if (cfg.rx_ba_win > max_ba) cfg.rx_ba_win = max_ba;
    }
    return cfg;
}

static xip_wifi_tuning current_tuning(void)
{
    xip_wifi_tuning t = {.size = sizeof(t)};
    if (xip_wifi_get_tuning(&t)) memset(&t, 0, sizeof(t)), t.size = sizeof(t);
    return t;
}

/* ---------------------------------------------------------------------------------------------------------------------------------
 * The radio bring-up. This follows DMesh's wifi_esp.rs (init_sta, init_nan_now, start_sta_driver_once, stop_sta) step by step, in the
 * same order and with the same driver settings, because that sequence went through a lot of testing on the boards. The Rust twin is
 * crates/xip-runtime-esp/src/wifi_esp.rs and the two are meant to behave the same. What DMesh decides by policy (scan-then-choose of
 * the access point, the reconnect task, its fixed AP name, NAN discovery windows) is not here: the wanted state comes from the `wifi`
 * intent, a failed association is retried, and the ESP-NOW link (xip_link_espnow.c) is started with the radio.
 * --------------------------------------------------------------------------------------------------------------------------------- */

/* 802.11 b/g/n, as DMesh sets on the STA: n lets the association negotiate HT20 and AMPDU (no n-only 2.4 GHz bitmap exists). */
#define STA_PROTOCOL (WIFI_PROTOCOL_11B | WIFI_PROTOCOL_11G | WIFI_PROTOCOL_11N)
#define STA_HT40 false                /* HT20: the data lane must coexist with NOW on 2.4 GHz */
#define AP_BEACON_TU 500              /* ~512 ms, a supported SoftAP interval: the timing fallback of the soft cluster */
#define DEFAULT_CHANNEL 6             /* the channel of an unassociated radio when the intent names none: DMesh's out-of-box NOW channel */
#define REASON_STA_LEAVING 36         /* after our own esp_wifi_disconnect: not an association failure */

esp_err_t nvs_flash_init(void);
/* private libpp receive-filter hooks, the same declarations as DMesh: the STA lane is 0 */
void ic_rx_disable_bssid_check(uint8_t interface_id);
void ic_rx_enable_bssid_check(uint8_t interface_id);
bool ic_set_rx_policy_ubssid_check(uint8_t interface_id, bool enabled);

static esp_netif_t *sta_netif, *ap_netif;
static bool started;                  /* the radio is up for the wanted state */
static bool connected;                /* the station has an address */
static bool associated;               /* the station is associated (the driver's transition is the authority) */
static bool owner_disconnect_pending;
static bool want_sta, ap_enabled;
static bool scan_only, scanning, scan_done;
static char active_ssid[33];
/* the driver epoch: it stays initialized across radio epochs; a changed pre-start policy (AMPDU, 11b rates) is consumed by a full re-init */
static bool driver_initialized, driver_running, driver_ampdu = true, driver_11b_disabled = true;
static bool handlers_registered, phy_nvs_initialized;
static uint8_t applied_channel;       /* only a channel this owner applied (esp_wifi_get_channel can fail while unassociated) */
static uint8_t applied[sizeof(xip_wifi_intent)];
static xip_wifi_tuning applied_tuning;

bool xip_wifi_interface_mac(wifi_interface_t interface, uint8_t mac[6]) { return esp_wifi_get_mac(interface, mac) == ESP_OK; }
bool xip_wifi_associated(void) { return associated; }
bool xip_wifi_ap_only_active(void) { return ap_enabled && !associated; }

/* The channel and secondary channel the radio is on, as DMesh reports it for action transmit. */
bool xip_wifi_current_channel(uint8_t *channel, wifi_second_chan_t *secondary)
{
    if (!associated && applied_channel >= 1 && applied_channel <= 13) {         /* unassociated: the selected channel is authoritative */
        *channel = applied_channel; *secondary = WIFI_SECOND_CHAN_NONE;
        return true;
    }
    uint8_t ch = 0; wifi_second_chan_t second = WIFI_SECOND_CHAN_NONE;
    if (esp_wifi_get_channel(&ch, &second) == ESP_OK && ch >= 1 && ch <= 13) {
        applied_channel = ch; *channel = ch; *secondary = second;
        return true;
    }
    if (applied_channel >= 1 && applied_channel <= 13) { *channel = applied_channel; *secondary = WIFI_SECOND_CHAN_NONE; return true; }
    return false;
}

static bool set_ht20_channel(uint8_t channel);
bool xip_wifi_set_channel(uint8_t channel) { return set_ht20_channel(channel); }

static bool set_ht20_channel(uint8_t channel)
{
    if (channel < 1) channel = 1;
    if (channel > 13) channel = 13;
    if (esp_wifi_set_channel(channel, WIFI_SECOND_CHAN_NONE) != ESP_OK) return false;
    applied_channel = channel;
    return true;
}

/* The private BSSID policy of one receive lane (DMesh's set_bssid_check_disabled): disabled, the radio hands up frames of other BSSIDs,
 * which unassociated ESP-NOW receive needs. */
static bool set_bssid_check_disabled(uint8_t interface_id, bool disabled)
{
    if (disabled) {
        bool updated = ic_set_rx_policy_ubssid_check(interface_id, false);
        ic_rx_disable_bssid_check(interface_id);
        return updated;
    }
    ic_rx_enable_bssid_check(interface_id);
    return ic_set_rx_policy_ubssid_check(interface_id, true);
}

/* PHY calibration requires an initialized NVS partition even though the driver never loads or saves a persisted configuration. */
static bool initialize_phy_nvs(void)
{
    if (phy_nvs_initialized) return true;
    esp_err_t rc = nvs_flash_init();
    if (rc == ESP_OK || rc == ESP_ERR_INVALID_STATE) { phy_nvs_initialized = true; return true; }
    return false;
}

static void ensure_link_local(esp_netif_t *netif)
{
    if (netif) (void)esp_netif_create_ip6_linklocal(netif);        /* the netif must be up: the START events arrive after it is */
}

static void on_wifi_event(void *arg, esp_event_base_t base, int32_t event, void *data)
{
    (void)arg;
    if (base == WIFI_EVENT && event == WIFI_EVENT_SCAN_DONE) {
        scan_done = true;
    } else if (base == WIFI_EVENT && event == WIFI_EVENT_STA_START) {
        ensure_link_local(sta_netif);
    } else if (base == WIFI_EVENT && event == WIFI_EVENT_AP_START) {
        ensure_link_local(ap_netif);
    } else if (base == WIFI_EVENT && event == WIFI_EVENT_STA_CONNECTED) {
        ensure_link_local(sta_netif);
        associated = true;
        if (data) xip_rx_filter_bssid(((wifi_event_sta_connected_t *)data)->bssid);
    } else if (base == WIFI_EVENT && event == WIFI_EVENT_STA_DISCONNECTED) {
        uint8_t reason = data ? ((wifi_event_sta_disconnected_t *)data)->reason : 0;
        bool owner_leave = owner_disconnect_pending;
        owner_disconnect_pending = false;
        bool intentional = reason == REASON_STA_LEAVING || (owner_leave && (reason == 3 || reason == 8));
        associated = connected = false;
        xip_rx_filter_bssid(NULL);
        /* DMesh's Main decides when to look for the network again; the simple owner retries the configured one */
        if (!intentional && started && want_sta) esp_wifi_connect();
    } else if (base == IP_EVENT && event == IP_EVENT_STA_GOT_IP) {
        connected = true;
    }
}

/* Subscribe once to the driver's events: a stop/start keeps the default event loop, so registering again would duplicate every transition. */
static bool register_event_handlers(void)
{
    if (handlers_registered) return true;
    if (esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, on_wifi_event, NULL) != ESP_OK ||
        esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_wifi_event, NULL) != ESP_OK) return false;
    handlers_registered = true;
    return true;
}

/* The platform setup before esp_wifi_init, in DMesh's order: PHY NVS, netif, default event loop. */
static bool platform_init(void)
{
    if (!initialize_phy_nvs()) return false;
    esp_err_t rc = esp_netif_init();
    if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return false;
    rc = esp_event_loop_create_default();
    return rc == ESP_OK || rc == ESP_ERR_INVALID_STATE;
}

/* A changed AMPDU or 11b policy is only consumed by re-initializing the driver (DMesh's prepare_sta_driver_policy). */
static bool driver_policy_changed(bool ampdu, bool rates_11b_disabled)
{
    return driver_initialized && (driver_ampdu != ampdu || driver_11b_disabled != rates_11b_disabled);
}

/* esp_wifi_init once per driver epoch. */
static bool init_driver(bool ampdu, bool rates_11b_disabled, const xip_wifi_tuning *tuning)
{
    if (!driver_initialized) {
        driver_initialized = true;
        driver_ampdu = ampdu; driver_11b_disabled = rates_11b_disabled;
        wifi_init_config_t init = init_config(tuning);
        esp_err_t rc = esp_wifi_init(&init);
        if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) { driver_initialized = false; return false; }
    }
    (void)esp_wifi_set_storage(WIFI_STORAGE_RAM);
    return true;
}

static esp_err_t start_driver_once(void)
{
    if (driver_running) return ESP_OK;
    esp_err_t rc = esp_wifi_start();
    if (rc == ESP_OK) driver_running = true;
    return rc;
}

/* The soft AP, configured before the one Wi-Fi start of an AP+STA epoch (DMesh's configure_unassociated_dmesh_ap, with the name and
 * passphrase of the intent in place of its fixed ones). */
static bool configure_ap(const xip_wifi_intent *p, uint8_t channel)
{
    if (!ap_netif) {
        ap_netif = esp_netif_create_default_wifi_ap();
        if (!ap_netif) return false;
    }
    wifi_config_t config = {0};
    const bool open = p->ap_psk_len == 0;
    if (p->ap_ssid_len) {
        memcpy(config.ap.ssid, p->ap_ssid, p->ap_ssid_len); config.ap.ssid_len = p->ap_ssid_len;
    } else {
        uint8_t mac[6]; esp_wifi_get_mac(WIFI_IF_AP, mac);       /* the default name, as the Rust twin: xip-<last two bytes of the AP MAC> */
        config.ap.ssid_len = snprintf((char *)config.ap.ssid, sizeof(config.ap.ssid), "xip-%02x%02x", mac[4], mac[5]);
    }
    if (!open) memcpy(config.ap.password, p->ap_psk, p->ap_psk_len);
    config.ap.channel = channel;
    config.ap.authmode = open ? WIFI_AUTH_OPEN : WIFI_AUTH_WPA2_PSK;
    /* Android's app-scoped WPA2 requests must not require PMF, but accepting it lets modern Android devices negotiate it */
    config.ap.pmf_cfg.capable = !open;
    config.ap.pmf_cfg.required = false;
    config.ap.max_connection = 4;
    config.ap.beacon_interval = AP_BEACON_TU;
    return esp_wifi_set_config(WIFI_IF_AP, &config) == ESP_OK;
}

static void restart_driver(void)
{
    (void)esp_wifi_deinit();
    driver_initialized = driver_running = false;
    if (sta_netif) { esp_netif_destroy_default_wifi(sta_netif); sta_netif = NULL; }
}

static void wifi_down(void);

/* Bring the radio up for the wanted state: the station (and its association), the soft AP, and ESP-NOW (always, with the radio).
 * DMesh's init_sta / init_nan_now, step by step. */
static int wifi_up(const xip_wifi_intent *p, const xip_wifi_tuning *tuning)
{
    if (started || scan_only) return -5;
    const bool sta = p->sta, ap = p->ap;
    /* DMesh's defaults: AMPDU on, 11b rates off, BSSID check off (frames of other BSSIDs reach unassociated NOW receive) */
    const bool ampdu = tuning->ampdu != XIP_TUNE_OFF;
    const bool rates_11b_disabled = tuning->rates_11b != XIP_TUNE_ON;
    const bool bssid_check_disabled = tuning->bssid_check != XIP_TUNE_ON;
    const uint8_t channel = p->channel ? (p->channel > 13 ? 13 : p->channel) : DEFAULT_CHANNEL;
    if (driver_policy_changed(ampdu, rates_11b_disabled)) { wifi_down(); restart_driver(); }
    if (!platform_init()) return -3;
    /* The standard ESP-IDF ordering: create the default STA glue after esp-netif and the event loop but before esp_wifi_init
     * (esp_wifi_internal_tx is the glue's egress API; creating it later accepted frames but left the Ethernet path undrained). */
    if (!sta_netif) {
        sta_netif = esp_netif_create_default_wifi_sta();
        if (!sta_netif) return -6;
    }
    /* the default handlers bring the netif up before our callbacks create IPv6 link-local */
    if (!register_event_handlers()) return -3;
    if (!init_driver(ampdu, rates_11b_disabled, tuning)) return -3;
    esp_err_t rc = esp_wifi_set_mode(ap ? WIFI_MODE_APSTA : WIFI_MODE_STA);
    if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return -3;
    if (ap && !configure_ap(p, channel)) return -3;
    if (sta) {
        wifi_config_t config = {0};
        memcpy(config.sta.ssid, p->sta_ssid, p->sta_ssid_len);
        if (p->sta_psk_len) {
            memcpy(config.sta.password, p->sta_psk, p->sta_psk_len);
            /* a configured WPA credential. DMesh sets the threshold to what its scan found (WPA2: WPA2_PSK, PMF capable); without the scan the lowest
             * it accepts is WPA2, so a WPA2-only AP, a transition AP and a WPA3 AP all join (a WPA2_WPA3 threshold rejects WPA2-only) */
            config.sta.threshold.authmode = WIFI_AUTH_WPA2_PSK;
            config.sta.pmf_cfg.capable = true;
            config.sta.pmf_cfg.required = false;
            config.sta.sae_pwe_h2e = WPA3_SAE_PWE_BOTH;
        } else {
            config.sta.threshold.authmode = WIFI_AUTH_OPEN;
        }
        static const uint8_t none[6];
        config.sta.channel = p->channel;
        if (memcmp(p->bssid, none, 6)) { config.sta.bssid_set = 1; memcpy(config.sta.bssid, p->bssid, 6); }
        if (esp_wifi_set_config(WIFI_IF_STA, &config) != ESP_OK) return -3;
    }
    wifi_protocols_t protocols = {.ghz_2g = STA_PROTOCOL, .ghz_5g = 0};
    if (esp_wifi_set_protocols(WIFI_IF_STA, &protocols) != ESP_OK) return -3;
    if (esp_wifi_set_bandwidth(WIFI_IF_STA, STA_HT40 ? WIFI_BW40 : WIFI_BW20) != ESP_OK) return -3;
    /* after init and before start; it includes the negotiated legacy/basic-rate set, so it is never applied to a running driver */
    if (!driver_running && esp_wifi_config_11b_rate(WIFI_IF_STA, rates_11b_disabled) != ESP_OK) return -3;
    associated = connected = false;
    rc = start_driver_once();
    if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return -3;
    if (!set_bssid_check_disabled(0, bssid_check_disabled)) return -3;
    (void)esp_wifi_set_ps(WIFI_PS_NONE);
    want_sta = sta; ap_enabled = ap;
    if (sta) {
        memset(active_ssid, 0, sizeof(active_ssid));
        memcpy(active_ssid, p->sta_ssid, p->sta_ssid_len);
        started = true;
        (void)esp_wifi_connect();      /* the driver's own events report a failed attempt; the retry above covers it */
    } else if (ap) {
        /* APSTA selects its configured channel as it starts: read the live value instead of calling set_channel after start, which
         * would add a driver transition to the out-of-box NOW test */
        uint8_t primary = 0; wifi_second_chan_t secondary;
        if (esp_wifi_get_channel(&primary, &secondary) != ESP_OK || primary != channel) return -3;
        applied_channel = primary;
        started = true;
    } else {
        /* an unassociated ESP-NOW-only epoch stays unassociated and pins its channel */
        (void)esp_wifi_disconnect();
        vTaskDelay(5);
        if (!set_ht20_channel(channel)) return -3;
        started = true;
    }
    { uint8_t sm[6], am[6]; esp_wifi_get_mac(WIFI_IF_STA, sm); esp_wifi_get_mac(WIFI_IF_AP, am); xip_rx_filter_local(sm, ap ? am : NULL); }
    /* with the radio on, ESP-NOW is on (DMesh's start_sta_extensions, after the driver is live and the channel is pinned) */
    (void)xip_link_espnow_start();
    return 0;
}

/* Release the radio the way DMesh's stop_sta does: the NOW callback goes inert, the station leaves, the driver stops and the STA netif is
 * destroyed; the initialized driver is kept for the next epoch. */
static void wifi_down(void)
{
    if (scan_only) {
        scan_only = scanning = scan_done = false;
        driver_running = false;
        esp_wifi_stop();
        return;
    }
    if (!started) return;
    started = false;
    xip_rx_filter_local(NULL, NULL);
    xip_rx_filter_bssid(NULL);
    xip_link_promisc_stop();
    xip_link_espnow_stop();
    driver_running = false;
    scanning = scan_done = false;
    want_sta = ap_enabled = false;
    owner_disconnect_pending = associated;
    associated = connected = false;
    (void)esp_wifi_disconnect();
    (void)esp_wifi_stop();
    if (sta_netif) { esp_netif_destroy_default_wifi(sta_netif); sta_netif = NULL; }
    memset(active_ssid, 0, sizeof(active_ssid));
}
#define wifi_stop wifi_down

/* Promiscuous receive for a NAN module (xip_link_promisc.c); the radio must be up. */
static int wifi_monitor(const xip_wifi_monitor *m)
{
    if (!started) return XIP_ERR_UNSUPPORTED;
    return xip_link_promisc_set(m);
}

static int wifi_state(xip_wifi_state *st)
{
    uint32_t size = st->size;
    memset(st, 0, sizeof(*st)); st->size = size;
    if (!started) return 0;
    st->ap = ap_enabled;
    uint8_t channel; wifi_second_chan_t second;
    if (xip_wifi_current_channel(&channel, &second)) st->channel = channel;
    if (!want_sta) return 0;
    st->sta = connected ? 2 : 1;
    if (connected) {
        esp_netif_ip_info_t info;
        wifi_ap_record_t ap;
        if (esp_netif_get_ip_info(sta_netif, &info) == ESP_OK) memcpy(st->ip4, &info.ip.addr, 4);
        if (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) {
            st->rssi = ap.rssi; memcpy(st->bssid, ap.bssid, 6); st->channel = ap.primary;
            st->ssid_len = strnlen((const char *)active_ssid, 32); memcpy(st->ssid, active_ssid, st->ssid_len);
        }
    }
    return 0;
}

/* The scan works in every radio mode (a control plane decides the topology from it). The station interface exists in all of them; a scan
 * leaves the channel, so an unassociated radio is put back on the channel this owner pinned when it is done. */
static int wifi_scan(xip_ap_t *aps, size_t max)
{
    if (!started && !scan_only) {
        xip_wifi_tuning tuning = current_tuning();
        if (!platform_init() || !register_event_handlers()) return -3;
        if (!init_driver(true, true, &tuning)) return -3;
        if (esp_wifi_set_mode(WIFI_MODE_STA) != ESP_OK) return -3;
        if (start_driver_once() != ESP_OK) return -3;
        scan_only = true;
    }
    if (!scanning) {
        scan_done = false;
        esp_err_t rc = esp_wifi_scan_start(NULL, false);
        if (rc != ESP_OK) {
            if (scan_only) wifi_stop();
            return rc == ESP_ERR_WIFI_STATE ? -5 : -3;      /* busy joining: try again */
        }
        scanning = true;
        return -5;
    }
    if (!scan_done) return -5;
    scanning = scan_done = false;
    if (started && !associated && applied_channel) (void)esp_wifi_set_channel(applied_channel, WIFI_SECOND_CHAN_NONE);
    if (max > 16) max = 16;
    wifi_ap_record_t records[16];
    uint16_t count = max;
    if (esp_wifi_scan_get_ap_records(&count, records) != ESP_OK) {
        if (scan_only) wifi_stop();
        return -3;
    }
    for (uint16_t i = 0; i < count; ++i) {
        memset(&aps[i], 0, sizeof(aps[i]));
        size_t len = strnlen((const char *)records[i].ssid, 32);
        memcpy(aps[i].ssid, records[i].ssid, len);
        aps[i].ssid_len = len;
        memcpy(aps[i].bssid, records[i].bssid, 6);
        aps[i].rssi = records[i].rssi;
        aps[i].channel = records[i].primary;
        aps[i].auth = records[i].authmode;
    }
    if (scan_only) wifi_stop();
    return count;
}

/* The radio around a light sleep (xip_power.c): stopped the way an epoch ends; the classic ESP32 releases the driver too, or esp_light_sleep_start
 * returns at once. `applied` is forgotten, so reconcile brings back the wanted state with the same settings. */
static bool power_suspend(void)
{
    if (!started && !scan_only) return false;
    wifi_down();
#if CONFIG_IDF_TARGET_ESP32
    restart_driver();
#endif
    memset(applied, 0, sizeof(applied));
    memset(&applied_tuning, 0, sizeof(applied_tuning));
    return true;
}
static void reconcile(void);
static void power_resume(void) { reconcile(); }

static void reconcile(void)
{
    xip_wifi_intent p = {.size = sizeof(p)};
    uint32_t generation;
    if (xip_wifi_get_intent(&p, &generation)) return;
    xip_wifi_tuning t = current_tuning();
    if (!memcmp(applied, &p, sizeof(p)) && !memcmp(&applied_tuning, &t, sizeof(t))) return;
    wifi_down();
    memcpy(applied, &p, sizeof(p));
    applied_tuning = t;
    if (p.radio) (void)wifi_up(&p, &t);
}

void xip_wifi_install_hooks(xip_host_hooks_t *hooks)
{
    hooks->wifi_state = wifi_state;
    hooks->wifi_reconcile = reconcile;
    hooks->wifi_scan = wifi_scan;
    hooks->wifi_monitor = wifi_monitor;
    static const xip_power_radio power_radio = {.suspend = power_suspend, .resume = power_resume};
    xip_power_set_radio(&power_radio);
    if (!hooks->nv) hooks->nv = xip_wifi_nv;
}
