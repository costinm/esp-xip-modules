/* Shared generic wanted Wi-Fi state. Product applications may replace these hooks. */
#include <string.h>
#include "xip_runtime.h"
#include "freertos/FreeRTOS.h"
#include "nvs.h"

static portMUX_TYPE lock = portMUX_INITIALIZER_UNLOCKED;
static xip_wifi_intent desired;
static uint32_t generation;
static xip_wifi_tuning tuning = {.size = sizeof(xip_wifi_tuning)};

static bool text_ok(const uint8_t *s, size_t n, size_t max, size_t min)
{
    return n <= max && (n == 0 || n >= min) && !memchr(s, 0, n);
}

/* The rules of the component, and nothing else: lengths, channel, and a name to join. */
static int valid(const xip_wifi_intent *i)
{
    if (i->size < sizeof(*i) || i->radio > 1 || i->sta > 1 || i->ap > 1 || i->now > 1) return XIP_ERR_ARG;
    if (i->channel > 14) return XIP_ERR_ARG;
    if (!text_ok(i->sta_ssid, i->sta_ssid_len, 32, 1) || !text_ok(i->ap_ssid, i->ap_ssid_len, 32, 1)) return XIP_ERR_ARG;
    if (!text_ok(i->sta_psk, i->sta_psk_len, 63, 8) || !text_ok(i->ap_psk, i->ap_psk_len, 63, 8)) return XIP_ERR_ARG;
    static const uint8_t none[6];
    if (i->sta && !i->sta_ssid_len && !memcmp(i->bssid, none, 6)) return XIP_ERR_ARG;
    return 0;
}

static int get(xip_wifi_intent *i, uint32_t *gen)
{
    if (!i || !gen) return XIP_ERR_ARG;
    if (i->size < sizeof(*i)) return XIP_ERR_SIZE;
    portENTER_CRITICAL(&lock);
    *i = desired; *gen = generation;
    portEXIT_CRITICAL(&lock);
    return 0;
}

static int set(const xip_wifi_intent *i, uint32_t *gen)
{
    if (!i || !gen) return XIP_ERR_ARG;
    int rc = valid(i); if (rc) return rc;
    xip_wifi_intent next = *i; next.size = sizeof(next); memset(next.reserved, 0, sizeof(next.reserved));
    portENTER_CRITICAL(&lock);
    bool changed = memcmp(&desired, &next, sizeof(next)) != 0;
    if (changed) { desired = next; if (++generation == 0) generation = 1; }
    *gen = generation;
    portEXIT_CRITICAL(&lock);
    if (changed) xip_host_wifi_changed(*gen);
    return 0;
}

/* The settings of the C host: A-MPDU, 802.11b rates, the BSSID check and the buffers. The raw UDP6 path (raw receive, driver-less
 * transmit, raw rate) is DMesh's until the `udp6` link is ported. */
static int tuning_valid(const xip_wifi_tuning *t)
{
    if (t->size < sizeof(*t)) return XIP_ERR_ARG;
    if (t->ampdu > XIP_TUNE_OFF || t->rates_11b > XIP_TUNE_OFF || t->bssid_check > XIP_TUNE_OFF || t->lr > XIP_TUNE_OFF) return XIP_ERR_ARG;
    if (t->raw_rx || t->driver_tx || t->raw_tx_rate) return XIP_ERR_UNSUPPORTED;
    if ((t->rx_static && (t->rx_static < 2 || t->rx_static > 64)) || (t->rx_dynamic && (t->rx_dynamic < 2 || t->rx_dynamic > 128)) ||
        (t->tx_dynamic && (t->tx_dynamic < 4 || t->tx_dynamic > 128)) || (t->rx_ba && (t->rx_ba < 2 || t->rx_ba > 64)))
        return XIP_ERR_ARG;
    return 0;
}

static int tuning_get(xip_wifi_tuning *t)
{
    if (!t) return XIP_ERR_ARG;
    if (t->size < sizeof(*t)) return XIP_ERR_SIZE;
    portENTER_CRITICAL(&lock);
    *t = tuning;
    portEXIT_CRITICAL(&lock);
    /* the switches this host has report their value in force, DMesh's defaults: A-MPDU on, 802.11b rates off, BSSID check off */
    t->lr = t->lr == XIP_TUNE_ON ? XIP_TUNE_ON : XIP_TUNE_OFF;
    t->ampdu = t->ampdu == XIP_TUNE_OFF ? XIP_TUNE_OFF : XIP_TUNE_ON;
    t->rates_11b = t->rates_11b == XIP_TUNE_ON ? XIP_TUNE_ON : XIP_TUNE_OFF;
    t->bssid_check = t->bssid_check == XIP_TUNE_ON ? XIP_TUNE_ON : XIP_TUNE_OFF;
    return 0;
}

static int tuning_set(const xip_wifi_tuning *t)
{
    int rc = t ? tuning_valid(t) : XIP_ERR_ARG; if (rc) return rc;
    xip_wifi_tuning next = *t; next.size = sizeof(next); next.reserved = 0;
    uint32_t gen = 0;
    portENTER_CRITICAL(&lock);
    bool changed = memcmp(&tuning, &next, sizeof(next)) != 0;
    if (changed) { tuning = next; if (++generation == 0) generation = 1; }
    gen = generation;
    portEXIT_CRITICAL(&lock);
    if (changed) xip_host_wifi_changed(gen);
    return 0;
}

void xip_wifi_intent_install(xip_host_hooks_t *h)
{
    desired.size = sizeof(desired);
    /* The simple STA owner uses the existing boot settings, never a stored intent. */
    if (h->wifi_state) {
        nvs_handle_t settings, secrets;
        if (nvs_open("dmesh", NVS_READONLY, &settings) == ESP_OK) {
            char name[33];
            size_t n = sizeof(name);
            esp_err_t rc = nvs_get_str(settings, "sta_ssid", name, &n);
            nvs_close(settings);
            if (rc == ESP_OK && n > 1 && n <= sizeof(name) && nvs_open("sec", NVS_READONLY, &secrets) == ESP_OK) {
                char psk[64];
                size_t m = sizeof(psk);
                rc = nvs_get_str(secrets, "sta", psk, &m); nvs_close(secrets);
                if (rc == ESP_OK && m >= 9 && m <= sizeof(psk)) {
                    memcpy(desired.sta_ssid, name, n - 1); desired.sta_ssid_len = n - 1;
                    memcpy(desired.sta_psk, psk, m - 1); desired.sta_psk_len = m - 1;
                    /* the saved credentials predate the `now` flag: keep the station + ESP-NOW the radio ran with */
                    desired.radio = 1; desired.sta = 1; desired.now = 1;
                }
            }
        }
    }
    if (!h->wifi_set) { h->wifi_set = set; h->wifi_get = get; }
    if (!h->wifi_tuning_set) { h->wifi_tuning_set = tuning_set; h->wifi_tuning_get = tuning_get; }
}
