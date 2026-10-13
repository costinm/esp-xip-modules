/* SPDX-License-Identifier: Apache-2.0 */
/* esp-xip-modules main: an example and test driver. It hosts signed modules and starts the installed ones that ask to
 * run at boot (their manifest says so: a debug console is one such module; a product installs its own and no console).
 * Everything else is in the components. What a product built on this does differently: its slot table, its services and
 * what it does with module events. */
#include <stdio.h>
#include <string.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "nvs_flash.h"
#include "esp_wifi_loader.h"
#include "espmod_host.h"
#include "espmod_install.h"
#include "espmod_loader.h"

static const char *TAG = "main";

/* Every module of this repository, flashed into its slot (tools/mkmod.py reads this table). Until the loader records
 * which module lives where, a new module means a new line here. */
const espmod_slot_t main_slots[] = {
    {"console", 20, 0x00000, 0x10000}, /* modules/mod_console: the debug front end; the module is optional, the slot is not */
    {"test",  2, 0x10000, 0x10000},   /* modules/mod_test (C) or modules/mod_rtest (Rust): the same slot and tag, installed in turn */
    {"lora",  43, 0x20000, 0x10000},   /* modules/mod_lora (Rust); the tags are the component numbers of DMesh */
    {"hw",    45, 0x30000, 0x10000},   /* modules/mod_hw (Rust) */
    /* room for more (flash: data partition 0x80000 = eight 64 KB pages); placeholders until the loader records the table */
    {"app5",  5, 0x40000, 0x10000}, {"app6", 6, 0x50000, 0x10000}, {"app7", 7, 0x60000, 0x10000}, {"app8", 8, 0x70000, 0x10000},
};

/* Services modules can call (mod_test uses both). */
enum { SERVICE_ECHO = 1, SERVICE_UPTIME_MS = 2 };

/* the scan record of the loader and of the host hook are the same layout */
_Static_assert(sizeof(esp_wifi_loader_ap_t) == sizeof(espmod_ap_t), "scan record layout");
static int wifi_scan(espmod_ap_t *aps, size_t max) { return esp_wifi_loader_wifi_scan((esp_wifi_loader_ap_t *)aps, max); }

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

void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    /* The network, the uploader and the module list are other components' functions, offered to modules through the ABI. */
    const espmod_host_hooks_t hooks = {
        .call_service = on_service,
        .wifi_start = esp_wifi_loader_wifi_start, .wifi_stop = esp_wifi_loader_wifi_stop,
        .wifi_status = esp_wifi_loader_wifi_status, .wifi_scan = wifi_scan,
        .wifi_config = esp_wifi_loader_network_set, .wifi_saved = esp_wifi_loader_network_get, .nv = esp_wifi_loader_nv,
        .loader_start = esp_wifi_loader_server_start, .loader_stop = esp_wifi_loader_server_stop,
        .modules_info = espmod_loader_modules_info, .module_start = espmod_loader_start_named,
        .module_stop = espmod_loader_stop_named, .module_manifest = espmod_loader_manifest_named,
    };
    ESP_ERROR_CHECK(espmod_host_init(&hooks));
    const espmod_loader_config_t cfg = {.slots = main_slots, .slot_count = sizeof(main_slots) / sizeof(main_slots[0]),
                                        .partition = "data", .default_stack_bytes = 8192};
    ESP_ERROR_CHECK(espmod_loader_init(&cfg));
    ESP_ERROR_CHECK(espmod_install_init());

    /* What runs at boot is what is installed and asks for it in its manifest (the console module, if it is installed). */
    espmod_loader_autostart();
    ESP_LOGW(TAG, "ready");
}
