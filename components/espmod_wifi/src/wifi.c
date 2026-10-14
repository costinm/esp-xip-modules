/* SPDX-License-Identifier: Apache-2.0 */
#include <stdio.h>
#include <string.h>
#include "espmod_wifi.h"
#include "espmod_cbor.h"
#include "nvs.h"
#include "esp_event.h"
#include "esp_netif.h"

static uint16_t setting(nvs_handle_t nvs, const char *key, uint16_t def, uint16_t lo, uint16_t hi)
{
    uint16_t value = def;
    if (!nvs || nvs_get_u16(nvs, key, &value) != ESP_OK) value = def;
    return value < lo ? lo : value > hi ? hi : value;
}

void espmod_wifi_apply_init_settings(wifi_init_config_t *cfg)
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

int espmod_wifi_nv(const char *key, size_t key_len, const uint8_t *value, size_t value_len,
                   uint8_t *out, size_t cap)
{
    static const struct { const char *name, *key, *ns; bool secret; } strings[] = {
        {"sta_ssid", "sta_ssid", "dmesh", false}, {"sec:sta", "sta", "sec", true},
        {"uart.baud", "uart.baud", "dmesh", false}, {"uart.mode", "uart.mode", "dmesh", false},
        {"nan_dw_interval", "nan_dw_interval", "dmesh", false}, {"mode", "mode", "dmesh", false},
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
            case 2: valid = !strcmp(text,"115200") || !strcmp(text,"230400") || !strcmp(text,"460800") || !strcmp(text,"921600"); break;
            case 3: valid = !strcmp(text,"ppp") || !strcmp(text,"line") || !strcmp(text,"off"); break;
            case 4: valid = !strcmp(text,"1") || !strcmp(text,"8") || !strcmp(text,"16"); break;
            case 5: valid = !strcmp(text,"active") || !strcmp(text,"sleepy") || !strcmp(text,"sleepy-soft"); break;
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

wifi_init_config_t espmod_wifi_init_config(bool ampdu_enabled)
{
    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    cfg.nvs_enable = 0;
    if (!ampdu_enabled) {
        cfg.ampdu_rx_enable = 0;
        cfg.ampdu_tx_enable = 0;
        cfg.rx_ba_win = 0;
    }
    espmod_wifi_apply_init_settings(&cfg);
    return cfg;
}

static esp_netif_t *sta_netif;
static bool started;
static bool connected;
static bool scan_only;
static bool scanning;
static bool scan_done;
static char active_ssid[33];

static void on_wifi_event(void *arg, esp_event_base_t base, int32_t event, void *data)
{
    (void)arg; (void)data;
    if (base == WIFI_EVENT && event == WIFI_EVENT_SCAN_DONE) {
        scan_done = true;
    } else if (base == WIFI_EVENT && event == WIFI_EVENT_STA_DISCONNECTED) {
        connected = false;
        if (started) esp_wifi_connect();
    } else if (base == IP_EVENT && event == IP_EVENT_STA_GOT_IP) {
        connected = true;
    }
}

static int wifi_start(const espmod_bearer_profile *p)
{
    if (started || scan_only) return -5;
    if (!p->ssid_len) return -1;
    char name[33] = {0}, secret[64] = {0};
    memcpy(name, p->ssid, p->ssid_len);
    memcpy(secret, p->sta_passphrase, p->sta_passphrase_len);
    esp_err_t rc = esp_netif_init();
    if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return -3;
    rc = esp_event_loop_create_default();
    if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return -3;
    sta_netif = esp_netif_create_default_wifi_sta();
    if (!sta_netif) return -6;
    wifi_init_config_t init = espmod_wifi_init_config(p->sta_ampdu_enabled);
    rc = esp_wifi_init(&init);
    if (rc != ESP_OK) goto fail_netif;
    rc = esp_wifi_set_storage(WIFI_STORAGE_RAM);
    if (rc != ESP_OK) goto fail_driver;
    rc = esp_event_handler_register(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, on_wifi_event, NULL);
    if (rc != ESP_OK) goto fail_driver;
    rc = esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, on_wifi_event, NULL);
    if (rc != ESP_OK) goto fail_wifi_event;
    rc = esp_event_handler_register(WIFI_EVENT, WIFI_EVENT_SCAN_DONE, on_wifi_event, NULL);
    if (rc != ESP_OK) goto fail_events;
    wifi_config_t config = {0};
    memcpy(config.sta.ssid, name, p->ssid_len);
    config.sta.channel = p->sta_channel; config.sta.bssid_set = p->sta_bssid_set;
    memcpy(config.sta.bssid, p->sta_bssid, 6);
    memcpy(config.sta.password, secret, strlen(secret));
    rc = esp_wifi_set_mode(WIFI_MODE_STA);
    if (rc == ESP_OK) rc = esp_wifi_set_config(WIFI_IF_STA, &config);
    if (rc == ESP_OK) rc = esp_wifi_start();
    if (rc != ESP_OK) goto fail_events;
    /* Match DMesh's active STA initialization. Product scheduling may select
     * a power-save mode later; startup must not inherit the SDK default. */
    rc = esp_wifi_set_ps(WIFI_PS_NONE);
    if (rc != ESP_OK) { esp_wifi_stop(); goto fail_events; }
    memcpy(active_ssid, name, sizeof(name));
    connected = false;
    started = true;
    rc = esp_wifi_connect();
    if (rc == ESP_OK) return 0;
    started = false;
    esp_wifi_stop();
fail_events:
    esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_SCAN_DONE, on_wifi_event);
    esp_event_handler_unregister(IP_EVENT, IP_EVENT_STA_GOT_IP, on_wifi_event);
fail_wifi_event:
    esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, on_wifi_event);
fail_driver:
    esp_wifi_deinit();
fail_netif:
    esp_netif_destroy_default_wifi(sta_netif);
    sta_netif = NULL;
    return -3;
}

static void wifi_stop(void)
{
    if (scan_only) {
        scan_only = scanning = scan_done = false;
        esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_SCAN_DONE, on_wifi_event);
        esp_wifi_stop();
        esp_wifi_deinit();
        return;
    }
    if (!started) return;
    started = false;
    connected = false;
    scanning = scan_done = false;
    active_ssid[0] = 0;
    esp_wifi_disconnect();
    esp_wifi_stop();
    esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, on_wifi_event);
    esp_event_handler_unregister(IP_EVENT, IP_EVENT_STA_GOT_IP, on_wifi_event);
    esp_event_handler_unregister(WIFI_EVENT, WIFI_EVENT_SCAN_DONE, on_wifi_event);
    esp_wifi_deinit();
    esp_netif_destroy_default_wifi(sta_netif);
    sta_netif = NULL;
}

static int wifi_status(int *state, char ssid[33], char ip[16], int *rssi)
{
    *state = !started ? 0 : connected ? 2 : 1;
    memcpy(ssid, active_ssid, sizeof(active_ssid));
    ip[0] = 0;
    *rssi = 0;
    if (connected) {
        esp_netif_ip_info_t info;
        wifi_ap_record_t ap;
        if (esp_netif_get_ip_info(sta_netif, &info) == ESP_OK)
            snprintf(ip, 16, IPSTR, IP2STR(&info.ip));
        if (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) *rssi = ap.rssi;
    }
    return 0;
}

static int wifi_scan(espmod_ap_t *aps, size_t max)
{
    if (started && !connected) return -5;
    if (!started && !scan_only) {
        esp_err_t netif_rc = esp_netif_init();
        if (netif_rc != ESP_OK && netif_rc != ESP_ERR_INVALID_STATE) return -3;
        esp_err_t rc = esp_event_loop_create_default();
        if (rc != ESP_OK && rc != ESP_ERR_INVALID_STATE) return -3;
        wifi_init_config_t init = espmod_wifi_init_config(true);
        if (esp_wifi_init(&init) != ESP_OK) return -3;
        if (esp_wifi_set_storage(WIFI_STORAGE_RAM) != ESP_OK ||
            esp_wifi_set_mode(WIFI_MODE_STA) != ESP_OK || esp_wifi_start() != ESP_OK) {
            esp_wifi_deinit();
            return -3;
        }
        if (esp_event_handler_register(WIFI_EVENT, WIFI_EVENT_SCAN_DONE, on_wifi_event, NULL) != ESP_OK) {
            esp_wifi_stop(); esp_wifi_deinit(); return -3;
        }
        scan_only = true;
    }
    if (!scanning) {
        scan_done = false;
        if (esp_wifi_scan_start(NULL, false) != ESP_OK) {
            if (scan_only) wifi_stop();
            return -3;
        }
        scanning = true;
        return -5;
    }
    if (!scan_done) return -5;
    scanning = scan_done = false;
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

static int desired_start(const uint8_t *ssid, size_t n, const uint8_t *psk, size_t m)
{
    if (n > 32 || m > 63 || (n && !ssid) || (m && !psk)) return ESPMOD_ERR_ARG;
    uint8_t bytes[128]; em_cbor_w w; em_cbor_init(&w, bytes, sizeof(bytes));
    em_cbor_map(&w, 1 + (n != 0) + (m != 0)); em_cbor_uint(&w, 1); em_cbor_uint(&w, 1);
    if (n) { em_cbor_uint(&w, 2); em_cbor_text(&w, (const char *)ssid, n); }
    if (m) { em_cbor_uint(&w, 17); em_cbor_text(&w, (const char *)psk, m); }
    uint32_t generation; return espmod_transport_set(bytes, w.len, &generation);
}
static void desired_stop(void)
{
    const uint8_t off[] = {0xa1, 1, 0}; uint32_t generation;
    (void)espmod_transport_set(off, sizeof(off), &generation);
}
static int desired_config(const uint8_t *ssid, size_t n, const uint8_t *psk, size_t m)
{
    if (n > 32 || m > 63 || (n && !ssid) || (m && !psk) || (m && m < 8)) return ESPMOD_ERR_ARG;
    if ((n && memchr(ssid, 0, n)) || (m && memchr(psk, 0, m))) return ESPMOD_ERR_ARG;
    char name[33] = {0}, secret[64] = {0}; if (n) memcpy(name, ssid, n); if (m) memcpy(secret, psk, m);
    nvs_handle_t settings, secrets;
    if (nvs_open("dmesh", NVS_READWRITE, &settings) != ESP_OK) return ESPMOD_ERR_IO;
    if (nvs_open("sec", NVS_READWRITE, &secrets) != ESP_OK) { nvs_close(settings); return ESPMOD_ERR_IO; }
    esp_err_t a = n ? nvs_set_str(settings, "sta_ssid", name) : nvs_erase_key(settings, "sta_ssid");
    esp_err_t b = m ? nvs_set_str(secrets, "sta", secret) : nvs_erase_key(secrets, "sta");
    int rc = (a == ESP_OK || a == ESP_ERR_NVS_NOT_FOUND) && (b == ESP_OK || b == ESP_ERR_NVS_NOT_FOUND)
        && nvs_commit(secrets) == ESP_OK && nvs_commit(settings) == ESP_OK ? 0 : ESPMOD_ERR_IO;
    nvs_close(secrets); nvs_close(settings); return rc;
}
static int desired_saved(char ssid[33], int *has_psk)
{
    nvs_handle_t settings; size_t n = 33;
    if (nvs_open("dmesh", NVS_READONLY, &settings) != ESP_OK) return 0;
    esp_err_t rc = nvs_get_str(settings, "sta_ssid", ssid, &n); nvs_close(settings);
    if (rc != ESP_OK) return 0;
    *has_psk = 0; nvs_handle_t secrets;
    if (nvs_open("sec", NVS_READONLY, &secrets) == ESP_OK) {
        size_t len = 0; *has_psk = nvs_get_str(secrets, "sta", NULL, &len) == ESP_OK && len > 1; nvs_close(secrets);
    }
    return n > 1;
}

static void reconcile(void)
{
    static espmod_bearer_profile applied;
    espmod_bearer_profile p = {.size = sizeof(p)}; uint32_t generation;
    if (espmod_transport_get(&p, &generation)) return;
    if (applied.mode == p.mode && applied.run_requested == p.run_requested &&
        applied.ssid_len == p.ssid_len && applied.sta_passphrase_len == p.sta_passphrase_len && applied.sta_ampdu_enabled == p.sta_ampdu_enabled &&
        applied.sta_channel == p.sta_channel && applied.sta_bssid_set == p.sta_bssid_set &&
        !memcmp(applied.sta_bssid, p.sta_bssid, 6) &&
        !memcmp(applied.ssid, p.ssid, p.ssid_len) && !memcmp(applied.sta_passphrase, p.sta_passphrase, p.sta_passphrase_len)) return;
    wifi_stop();
    if (p.run_requested && p.mode == 1 && wifi_start(&p)) return;
    applied = p;
}

void espmod_wifi_install_hooks(espmod_host_hooks_t *hooks)
{
    hooks->wifi_start = desired_start;
    hooks->wifi_stop = desired_stop;
    hooks->wifi_status = wifi_status;
    hooks->wifi_config = desired_config;
    hooks->transport_reconcile = reconcile;
    hooks->wifi_saved = desired_saved;
    hooks->wifi_scan = wifi_scan;
    if (!hooks->nv) hooks->nv = espmod_wifi_nv;
}
