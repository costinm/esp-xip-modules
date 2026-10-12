/* SPDX-License-Identifier: Apache-2.0 */
/* The module sink of esp-wifi-loader (esp_wifi_loader.h): open, head, begin, write.., commit, done, in that order,
 * one upload at a time (the loader's buffers are static). Everything about flash and running modules is
 * espmod_loader's slot_* calls; the minimum version (rollback protection) is kept in esp-wifi-loader's state log. */
#include "esp_log.h"
#include "esp_wifi_loader.h"
#include "espmod_install.h"
#include "espmod_loader.h"

static const char *TAG = "espmod-install";   /* used by init */
static struct { int slot; uint32_t version; bool begun; } cur = {.slot = -1};

static int map_error(int rc)
{
    switch (rc) {
    case 0: return 0;
    case ESPMOD_LOADER_ERR_IMAGE: return WL_ERR_BAD_IMAGE_HEADER;
    case ESPMOD_LOADER_ERR_TIMEOUT: return WL_ERR_BUSY;
    case ESPMOD_ERR_SIZE: return WL_ERR_BAD_LENGTH;
    default: return WL_ERR_FLASH;
    }
}

static int install_open(void *arg, const char *name, uint32_t version, uint32_t image_size, bool dry)
{
    (void)arg; (void)dry;
    cur.slot = -1; cur.begun = false;
    int slot = espmod_loader_find(name);
    if (slot < 0) return WL_ERR_UNKNOWN_MODULE;
    if (image_size < ESPMOD_DMOD_HEADER_SIZE + 4 || image_size > espmod_loader_slot(slot)->size) return WL_ERR_BAD_LENGTH;
    uint32_t minver;
    if (esp_wifi_loader_module_minver((unsigned)slot, &minver) && version < minver) return WL_ERR_ROLLBACK;
    cur.slot = slot;
    cur.version = version;
    return 0;
}

static int install_head(void *arg, const uint8_t *data, size_t len)
{
    (void)arg;
    return map_error(espmod_loader_check_head(cur.slot, data, len));
}

static int install_begin(void *arg)
{
    (void)arg;
    cur.begun = true;
    return map_error(espmod_loader_slot_begin(cur.slot));
}

static int install_write(void *arg, uint32_t offset, const uint8_t *data, size_t n)
{
    (void)arg;
    return map_error(espmod_loader_slot_write(cur.slot, offset, data, n));
}

static int install_commit(void *arg, const uint8_t *head, size_t len)
{
    (void)arg;
    /* Version first, head block last: if the version cannot be saved the install fails while the slot still has
     * no valid header. */
    if (esp_wifi_loader_set_module_minver((unsigned)cur.slot, cur.version)) return WL_ERR_FLASH;
    return map_error(espmod_loader_slot_commit(cur.slot, head, len, cur.version));
}

static void install_done(void *arg, bool installed, bool begun)
{
    (void)arg; (void)installed; (void)begun;
    if (cur.slot >= 0 && cur.begun) espmod_loader_slot_end(cur.slot);
    cur.slot = -1;
}

const esp_wifi_loader_module_sink_t *espmod_install_sink(void)
{
    static const esp_wifi_loader_module_sink_t sink = {
        .open = install_open, .head = install_head, .begin = install_begin,
        .write = install_write, .commit = install_commit, .done = install_done,
    };
    return &sink;
}

esp_err_t espmod_install_init(void)
{
    esp_err_t err = esp_wifi_loader_init();
    if (err != ESP_OK) return err;
    for (int i = 0; espmod_loader_slot(i); i++) {   /* show the version the state log keeps */
        uint32_t v;
        if (esp_wifi_loader_module_minver((unsigned)i, &v)) espmod_loader_set_version(i, v);
    }
    esp_wifi_loader_set_module_sink(espmod_install_sink());
    ESP_LOGI(TAG, "uploads go to %u slots", (unsigned)espmod_loader_slot_count());
    return ESP_OK;
}
