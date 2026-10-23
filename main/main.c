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

void app_main(void)
{
    xip_heap_checkpoint("app-entry");
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
    xip_heap_checkpoint("pre-network");
    xip_wifi_boot_network();
    xip_loader_autostart();
    ESP_LOGW(TAG, "ready");
}
