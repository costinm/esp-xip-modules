/* SPDX-License-Identifier: Apache-2.0 */
/* esp-xip-modules main: an example and test driver. It hosts modules and starts the installed ones that ask to
 * run at boot (their manifest says so: a debug console is one such module; a product installs its own and no console).
 * Everything else is in the components. What a product built on this does differently: its slot table, its services and
 * what it does with module events. */
#include <stdio.h>
#include "esp_log.h"
#include "nvs_flash.h"
#include "xip_runtime.h"
#include "xip_loader.h"
#include "xip_wifi.h"
#include "xip_ble.h"

static const char *TAG = "main";

/* Safe mode (the runtime crashed in a loop and Stage2 had no older one to boot): the modules that still autostart. Flash management stays so
 * a good runtime can be uploaded; Wi-Fi in station mode comes from xip_wifi_boot_network. No NAN, no BLE, no classifier. */
static const char *const SAFE_MODULES[] = {"flash", NULL};

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
        .module_stop = xip_loader_stop_named, .module_manifest = xip_loader_manifest_named, .module_rescan = xip_loader_rescan, .component_start = xip_loader_start_hosting, .request_start = xip_loader_start_request,
    };
    xip_wifi_install_hooks(&hooks);
    ESP_ERROR_CHECK(xip_host_init(&hooks));
    /* Offers get_component("nimble") and the CoC channel as the link "ble" when the image has BLE (CONFIG_XIP_BLE); nothing starts until a request. */
    (void)xip_ble_install();
    /* Native relay classification remains enabled; endpoint modules explicitly
     * attach their packet owner. No demonstration CIDs or consuming callbacks. */
    /* Safe mode is known only after the loader scans; the loader's init does not start anything, so it is asked before the rest starts. */
    const xip_loader_config_t cfg = {.partition = "xip", .default_stack_bytes = 8192, .safe_modules = SAFE_MODULES};
    ESP_ERROR_CHECK(xip_loader_init(&cfg));
    const bool safe = xip_loader_safe_mode();
    if (!safe) xip_quic_classifier_enable(true);
    /* The persistent setting ble.boot (module ble, method mode): 1 starts the service at boot. With RELEASE_WHEN_OFF a boot without it
     * gives the controller memory back to the heap (BLE then needs a reboot with the setting on). */
    {
        uint8_t v[4] = {0};
        xip_ble_configure_boot(!safe && xip_wifi_nv("ble.boot", 8, NULL, 0, v, sizeof v) > 0 && v[0] == '1');
    }
    /* What runs at boot is what is installed and asks for it in its manifest (the console module, if it is installed). */
    xip_heap_checkpoint("pre-network");
    xip_wifi_boot_network();
    xip_loader_autostart();
    /* A runtime that stays up for a minute is settled: the older image (still there while this one is on trial) is erased. */
    if (!safe) xip_loader_settle_after(60000);
    ESP_LOGW(TAG, "ready");
}
