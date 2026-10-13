/* SPDX-License-Identifier: Apache-2.0 */
/* Replacing the image of a slot: what an installer does, in this order (espmod_loader.h). Flash and nothing else: the
 * packages, the keys and the network are the installer's (components/espmod_install on esp-wifi-loader, or DMesh's own). */
#include <stdlib.h>
#include "esp_log.h"
#include "internal.h"

static const char *TAG = "espmod-slots";

void espmod_loader_set_version(int slot, uint32_t version)
{
    if (espmod_loader_slot(slot)) em.slots[slot].st.version = version;
}

#define ERASE_CHUNK 65536u

static bool part_erase(uint32_t off, size_t len)
{
    return esp_partition_erase_range(em.part, off, (len + 0xfff) & ~(size_t)0xfff) == ESP_OK;
}

int espmod_loader_check_head(int slot, const uint8_t *head, size_t len)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    if (len < sizeof(espmod_dmod_header)) return ESPMOD_LOADER_ERR_IMAGE;
    espmod_slot_rules_t rules = em_rules(slot);
    espmod_dmod_result_t r = espmod_dmod_check((const espmod_dmod_header *)head, &rules);
    if (r != EMD_OK) {
        ESP_LOGW(TAG, "%s: image header: %s", em.cfg.slots[slot].name, espmod_dmod_reason(r));
        return ESPMOD_LOADER_ERR_IMAGE;
    }
    return 0;
}

int espmod_loader_slot_begin(int slot)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    em_slot_state_t *st = &em.slots[slot];
    const espmod_slot_t *def = &em.cfg.slots[slot];
    if (st->st.needs_reboot) return ESPMOD_ERR_LEAK;       /* its last run lost memory: restart before replacing it */
    st->was_running = st->st.running;
    if (espmod_loader_stop(slot, 3000) != 0) return ESPMOD_LOADER_ERR_TIMEOUT;
    /* Invalidate first: from here until the final head write the slot has no valid header. */
    if (!part_erase(def->offset, def->size < ERASE_CHUNK ? def->size : ERASE_CHUNK)) return ESPMOD_LOADER_ERR_FLASH;
    st->st.installed = false;
    return 0;
}

int espmod_loader_slot_write(int slot, uint32_t offset, const uint8_t *data, size_t len)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    const espmod_slot_t *def = &em.cfg.slots[slot];
    if (offset + len > def->size) return ESPMOD_ERR_SIZE;
    if (offset % ERASE_CHUNK == 0 && !part_erase(def->offset + offset, ERASE_CHUNK)) return ESPMOD_LOADER_ERR_FLASH;
    return esp_partition_write(em.part, def->offset + offset, data, len) == ESP_OK ? 0 : ESPMOD_LOADER_ERR_FLASH;
}

int espmod_loader_slot_commit(int slot, const uint8_t *head, size_t len, uint32_t version)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    if (esp_partition_write(em.part, em.cfg.slots[slot].offset, head, len) != ESP_OK) return ESPMOD_LOADER_ERR_FLASH;
    em.slots[slot].st.installed = true;
    em.slots[slot].st.version = version;
    return 0;
}

void espmod_loader_slot_end(int slot)
{
    if (!espmod_loader_slot(slot)) return;
    em_slot_state_t *st = &em.slots[slot];
    if (!st->was_running || !st->st.installed) return;
    /* Hot replace (or a failure that left the old image): same request as before. */
    st->was_running = false;
    uint8_t *req = st->request;
    size_t req_len = st->request_len;
    st->request = NULL;
    int rc = espmod_loader_start(slot, req, req_len);
    free(req);
    if (rc) ESP_LOGW(TAG, "%s: restart failed: %d", em.cfg.slots[slot].name, rc);
}

