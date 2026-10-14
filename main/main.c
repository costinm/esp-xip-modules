/* SPDX-License-Identifier: Apache-2.0 */
/* esp-xip-modules main: an example and test driver. It hosts modules and starts the installed ones that ask to
 * run at boot (their manifest says so: a debug console is one such module; a product installs its own and no console).
 * Everything else is in the components. What a product built on this does differently: its slot table, its services and
 * what it does with module events. */
#include <stdio.h>
#include <string.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "nvs_flash.h"
#include "espmod_host.h"
#include "espmod_loader.h"
#include "espmod_wifi.h"
#include "espmod_transport.h"

static const char *TAG = "main";

/* Every module of this repository, flashed into its slot (tools/mkmod.py reads this table). Until the loader records
 * which module lives where, a new module means a new line here. */
const espmod_slot_t main_slots[] = {
#if CONFIG_ESPMOD_C_LOADER
#include "product-slots.inc"
#else
    {"console", 20, 0x00000, 0x10000}, /* modules/mod_console: the debug front end; the module is optional, the slot is not */
    {"test",  2, 0x10000, 0x10000},   /* modules/mod_test (C) or modules/mod_rtest (Rust): the same slot and tag, installed in turn */
    {"lora",  43, 0x20000, 0x10000},   /* modules/mod_lora (Rust); the tags are the component numbers of DMesh */
    {"hw",    45, 0x30000, 0x10000},   /* modules/mod_hw (Rust) */
    /* room for more (flash: data partition 0x80000 = eight 64 KB pages); placeholders until the loader records the table */
    {"app5",  5, 0x40000, 0x10000}, {"app6", 6, 0x50000, 0x10000}, {"app7", 7, 0x60000, 0x10000}, {"app8", 8, 0x70000, 0x10000},
#endif
};

/* Services modules can call (mod_test uses both). */
enum { SERVICE_ECHO = 1, SERVICE_UPTIME_MS = 2 };

static int on_service(void *ctx, const char *module, uint16_t tag, const uint8_t *req, size_t req_len,
                      uint8_t *resp, size_t cap, size_t *resp_len, uint32_t timeout_ms)
{
    (void)ctx; (void)module; (void)timeout_ms;
    if (tag == SERVICE_ECHO) {
        if (req_len > cap) return ESPMOD_ERR_SIZE;
        if (req_len) memcpy(resp, req, req_len);
        *resp_len = req_len;
        return 0;
    }
    if (tag == SERVICE_UPTIME_MS) {
        uint64_t ms = (uint64_t)(esp_timer_get_time() / 1000);
        if (cap < sizeof(ms)) return ESPMOD_ERR_SIZE;
        memcpy(resp, &ms, sizeof(ms));
        *resp_len = sizeof(ms);
        return 0;
    }
    return ESPMOD_ERR_NOTFOUND;
}

#if CONFIG_ESPMOD_C_LOADER
/* The C loader sample may use its existing wl credentials as a boot input.
 * This is a volatile request; it never creates a separate profile setting. */
static void loader_network(const espmod_host_hooks_t *hooks)
{
    espmod_bearer_profile profile = {.size = sizeof(profile)};
    uint32_t generation;
    if (!espmod_transport_get(&profile, &generation) && profile.ssid_len) return;
    char ssid[33] = {0}, secret[64] = {0};
    nvs_handle_t nvs;
    if (nvs_open("wl", NVS_READONLY, &nvs) != ESP_OK) return;
    size_t n = sizeof(ssid), m = sizeof(secret);
    esp_err_t rc = nvs_get_str(nvs, "ssid", ssid, &n);
    if (nvs_get_str(nvs, "psk", secret, &m) != ESP_OK) m = 1;
    nvs_close(nvs);
    if (rc != ESP_OK || n <= 1 || n > sizeof(ssid) || m > sizeof(secret)) return;
    /* Add the shared boot keys, preserving the loader's original keys. */
    if (!hooks->wifi_config || hooks->wifi_config((const uint8_t *)ssid, n-1,
                                                (const uint8_t *)secret, m-1)) return;
    uint8_t request[128]; em_cbor_w w; em_cbor_init(&w, request, sizeof(request));
    em_cbor_map(&w, 4);
    em_cbor_uint(&w, 1); em_cbor_uint(&w, 1);
    em_cbor_uint(&w, 2); em_cbor_text(&w, ssid, n-1);
    em_cbor_uint(&w, 17); em_cbor_text(&w, secret, m-1);
    em_cbor_uint(&w, 16); em_cbor_uint(&w, 0);
    int len = em_cbor_done(&w);
    if (len > 0) (void)espmod_transport_set(request, (size_t)len, &generation);
}

#endif

void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    /* Module lifecycle is exposed through generic host hooks. The optional STA
     * Wi-Fi driver is available to modules, but starts no radio at boot. */
    espmod_host_hooks_t hooks = {
        .call_service = on_service,
        .modules_info = espmod_loader_modules_info, .module_start = espmod_loader_start_named,
        .module_stop = espmod_loader_stop_named, .module_manifest = espmod_loader_manifest_named,
    };
    espmod_wifi_install_hooks(&hooks);
    ESP_ERROR_CHECK(espmod_host_init(&hooks));
    const espmod_loader_config_t cfg = {.slots = main_slots, .slot_count = sizeof(main_slots) / sizeof(main_slots[0]),
                                        .partition = "data", .default_stack_bytes = 8192};
    ESP_ERROR_CHECK(espmod_loader_init(&cfg));
    /* What runs at boot is what is installed and asks for it in its manifest (the console module, if it is installed). */
#if CONFIG_ESPMOD_C_LOADER
    loader_network(&hooks);
#endif
    espmod_loader_autostart();
    ESP_LOGW(TAG, "ready");
}
