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
#include "xip_runtime.h"
#include "xip_loader.h"
#include "xip_wifi.h"

static const char *TAG = "main";

/* Saved credentials are a boot input in either application partition.
 * This is a volatile request; it never creates a separate profile setting. */
static void boot_network(const xip_host_hooks_t *hooks)
{
    xip_wifi_intent now = {.size = sizeof(now)};
    uint32_t generation;
    if (!xip_wifi_get_intent(&now, &generation) && now.sta_ssid_len) return;
    char ssid[33] = {0}, secret[64] = {0};
    nvs_handle_t nvs;
    if (nvs_open("wl", NVS_READONLY, &nvs) != ESP_OK) return;
    size_t n = sizeof(ssid), m = sizeof(secret);
    esp_err_t rc = nvs_get_str(nvs, "ssid", ssid, &n);
    if (nvs_get_str(nvs, "psk", secret, &m) != ESP_OK) m = 1;
    nvs_close(nvs);
    if (rc != ESP_OK || n <= 1 || n > sizeof(ssid) || m > sizeof(secret)) return;
    /* Add the shared boot keys, preserving the loader's original keys. */
    if (hooks->nv) {
        (void)hooks->nv("sta_ssid", 8, (const uint8_t *)ssid, n-1, NULL, 0);
        (void)hooks->nv("sec:sta", 7, (const uint8_t *)secret, m-1, NULL, 0);
    }
    xip_wifi_intent want = {.size = sizeof(want), .radio = 1, .sta = 1};
    memcpy(want.sta_ssid, ssid, n-1); want.sta_ssid_len = n-1;
    memcpy(want.sta_psk, secret, m-1); want.sta_psk_len = m-1;
    (void)xip_wifi_set_intent(&want, &generation);
}

void app_main(void)
{
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    /* Module lifecycle and desired Wi-Fi state use the same hooks in either partition. */
    xip_host_hooks_t hooks = {
        .modules_info = xip_loader_modules_info, .module_start = xip_loader_start_named,
        .module_stop = xip_loader_stop_named, .module_manifest = xip_loader_manifest_named,
    };
    xip_wifi_install_hooks(&hooks);
    ESP_ERROR_CHECK(xip_host_init(&hooks));
    const xip_loader_config_t cfg = {.partition = "data", .default_stack_bytes = 8192};
    ESP_ERROR_CHECK(xip_loader_init(&cfg));
    /* What runs at boot is what is installed and asks for it in its manifest (the console module, if it is installed). */
    boot_network(&hooks);
    xip_loader_autostart();
    ESP_LOGW(TAG, "ready");
}
