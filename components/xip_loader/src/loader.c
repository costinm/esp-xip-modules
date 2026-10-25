/* SPDX-License-Identifier: Apache-2.0 */
/* Map a module image from flash, run it on its own task, stop it, report on it.
 *
 * Where the code runs depends on the chip:
 *  - ESP32 and ESP32-S3 (Xtensa): the toolchain cannot make position independent code, so each module
 *    is linked for one fixed address (code_base + its window slot, the index of its RAM cell, 64 KiB each) and the loader maps the
 *    flash page it was found on at exactly that address in both the instruction and the data alias (literals are data loads).
 *    The image may sit on any flash page. That needs the HAL MMU calls: esp_partition_mmap() never lets the caller choose the address.
 *  - ESP32-C6 (RISC-V): the image is position independent; esp_partition_mmap() is enough.
 * The fixed-address part is the proven code of DMesh's module loader (docs/architecture.md). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "esp_log.h"
#include "xip_cbor.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "internal.h"
#include "xip_module_ram.h"
#if CONFIG_IDF_TARGET_ESP32S3 || CONFIG_IDF_TARGET_ESP32
#include "esp_cache.h"
#include "hal/mmu_hal.h"
#include "hal/mmu_types.h"
#endif
#if CONFIG_IDF_TARGET_ESP32
#include "esp_attr.h"
#include "esp_private/cache_utils.h"
#include "esp_private/esp_cache_esp32_private.h"
#include "hal/cache_ll.h"
#include "hal/cache_types.h"
#endif

static const char *TAG = "xip-loader";
xip_loader_t em;

#if CONFIG_IDF_TARGET_ESP32S3
#define XIP_FIXED 1
#define XIP_CODE_BASE 0x43000000u
#define XIP_DATA_BASE 0x3d000000u
#define XIP_WINDOW_BYTES (XIP_DMOD_STATIC_SLOTS * XIP_DMOD_WINDOW_SLOT_BYTES)
#elif CONFIG_IDF_TARGET_ESP32
#define XIP_FIXED 1
/* Above Main's own IROM (0x400d0000..) and DROM. */
#define XIP_CODE_BASE 0x40300000u
#define XIP_DATA_BASE 0x3f700000u
#define XIP_WINDOW_BYTES (XIP_DMOD_STATIC_SLOTS * XIP_DMOD_WINDOW_SLOT_BYTES)
#else
#define XIP_FIXED 0
#define XIP_CODE_BASE 0u
#define XIP_DATA_BASE 0u
#define XIP_WINDOW_BYTES 0u
#endif


static xip_slot_rules_t rules_for(uint32_t offset, uint32_t size)
{
    return (xip_slot_rules_t){.slot_offset = offset, .slot_size = size, .max_stack_bytes = CONFIG_XIP_WORKER_STACK,
                                 .fixed_vma = XIP_FIXED, .code_base = XIP_CODE_BASE, .data_base = XIP_DATA_BASE, .window_bytes = XIP_WINDOW_BYTES,
                                 .static_base = XIP_MODULE_RAM_BASE};
}

xip_slot_rules_t xip_rules(int slot)
{
    const xip_slot_t *s = &em.slots_found[slot];
    return rules_for(s->offset, s->size);
}

esp_err_t xip_read_header(int slot, xip_dmod_header *h)
{
    return esp_partition_read(em.part, em.slots_found[slot].offset, h, sizeof(*h));
}

#if XIP_FIXED
#if CONFIG_IDF_TARGET_ESP32
/* IDF's esp_mmu_map() cannot take a caller-chosen address. Reproduce its small mapping critical
 * section for both aliases. Everything run while the caches are stopped is IRAM or inline HAL. */
static IRAM_ATTR NOINLINE_ATTR void map_aliases_esp32(uint32_t paddr, uint32_t len, uint32_t code_vaddr,
                                                      uint32_t data_vaddr, uint32_t *code_len, uint32_t *data_len)
{
    uint32_t mapped = 0;
    spi_flash_disable_interrupts_caches_and_other_cpu();
    mmu_hal_map_region(0, MMU_TARGET_FLASH0, code_vaddr, paddr, len, &mapped);
#if !CONFIG_ESP_SYSTEM_SINGLE_CORE_MODE
    mmu_hal_map_region(1, MMU_TARGET_FLASH0, code_vaddr, paddr, len, &mapped);
#endif
    *code_len = mapped;
    mmu_hal_map_region(0, MMU_TARGET_FLASH0, data_vaddr, paddr, len, &mapped);
#if !CONFIG_ESP_SYSTEM_SINGLE_CORE_MODE
    mmu_hal_map_region(1, MMU_TARGET_FLASH0, data_vaddr, paddr, len, &mapped);
#endif
    *data_len = mapped;
    cache_bus_mask_t buses = cache_ll_l1_get_bus(0, code_vaddr, len);
    buses |= cache_ll_l1_get_bus(0, data_vaddr, len);
    cache_ll_l1_enable_bus(0, buses);
#if !CONFIG_ESP_SYSTEM_SINGLE_CORE_MODE
    cache_ll_l1_enable_bus(1, buses);
#endif
    cache_sync();
    spi_flash_enable_interrupts_caches_and_other_cpu();
}
#endif

static uint32_t mmu_id(void)
{
#if SOC_MMU_PER_EXT_MEM_TARGET
    return mmu_hal_get_id_from_target(MMU_TARGET_FLASH0);
#else
    return 0;
#endif
}

static esp_err_t map_fixed(int slot, const xip_dmod_header *h, fixed_map_t *m)
{
    const uint32_t id = mmu_id(), page = mmu_hal_pages_to_bytes(id, 1);
    const uint32_t paddr = em.part->address + em.slots_found[slot].offset;
    const uint32_t rounded = ((h->image_size + page - 1) / page) * page;
    if (page == 0 || paddr % page || h->code_vma % page || h->data_vma % page || rounded > em.slots_found[slot].size)
        return ESP_ERR_INVALID_ARG;
    uint32_t code_len = 0, data_len = 0;
#if CONFIG_IDF_TARGET_ESP32
    map_aliases_esp32(paddr, h->image_size, h->code_vma, h->data_vma, &code_len, &data_len);
#else
    mmu_hal_map_region(id, MMU_TARGET_FLASH0, h->code_vma, paddr, h->image_size, &code_len);
    mmu_hal_map_region(id, MMU_TARGET_FLASH0, h->data_vma, paddr, h->image_size, &data_len);
#endif
    if (code_len < rounded || data_len < rounded) {
        mmu_hal_unmap_region(id, h->code_vma, code_len);
        mmu_hal_unmap_region(id, h->data_vma, data_len);
        return ESP_FAIL;
    }
#if !CONFIG_IDF_TARGET_ESP32
    /* A previous module may have occupied either alias: drop stale cache lines. */
    esp_err_t e = esp_cache_msync((void *)h->code_vma, code_len, ESP_CACHE_MSYNC_FLAG_DIR_M2C | ESP_CACHE_MSYNC_FLAG_TYPE_INST);
    if (e == ESP_OK) e = esp_cache_msync((void *)h->data_vma, data_len, ESP_CACHE_MSYNC_FLAG_DIR_M2C | ESP_CACHE_MSYNC_FLAG_TYPE_DATA);
    if (e != ESP_OK) {
        mmu_hal_unmap_region(id, h->code_vma, code_len);
        mmu_hal_unmap_region(id, h->data_vma, data_len);
        return e;
    }
#endif
    *m = (fixed_map_t){.code_vaddr = h->code_vma, .data_vaddr = h->data_vma, .mapped = code_len};
    return ESP_OK;
}

static void unmap_fixed(const fixed_map_t *m)
{
    mmu_hal_unmap_region(mmu_id(), m->code_vaddr, m->mapped);
    mmu_hal_unmap_region(mmu_id(), m->data_vaddr, m->mapped);
}
#endif

/* ---- mapping: the image stays mapped for as long as its instance lives (its handlers run on the host's worker) ---- */
static esp_err_t map_image(int slot, const xip_dmod_header *h, xip_slot_state_t *st, const void **base)
{
    *base = NULL;
#if XIP_FIXED
    if (map_fixed(slot, h, &st->fixed) != ESP_OK) return ESP_FAIL;
    *base = (const void *)(uintptr_t)st->fixed.code_vaddr;
    return ESP_OK;
#else
    if (esp_partition_mmap(em.part, em.slots_found[slot].offset, h->image_size, ESP_PARTITION_MMAP_INST, base,
                           &st->mmap) != ESP_OK) { *base = NULL; return ESP_FAIL; }
    return ESP_OK;
#endif
}

static void unmap_image(xip_slot_state_t *st)
{
#if XIP_FIXED
    unmap_fixed(&st->fixed);
#else
    esp_partition_munmap(st->mmap);
#endif
}

/* On the worker, after the instance is gone: nothing runs from the slot any more. */
static void instance_done(void *ctx, int result, const xip_stats_t *stats)
{
    const int page = (int)(intptr_t)ctx;     /* the page, not the slot index: a rescan may renumber the slots */
    xip_slot_state_t *st = &em.slots[page];
    unmap_image(st);
    xSemaphoreTake(em.lock, portMAX_DELAY);
    st->inst = NULL;                         /* from here no stop or post can reach it */
    st->starting = false;
    xSemaphoreGive(em.lock);
    st->st.stack_high_water_bytes = xip_host_stack_high_water();
    st->st.last_result = result;
    st->st.last_runtime_ms = (uint32_t)((esp_timer_get_time() - st->started_us) / 1000);
    st->st.runs++;
    if (stats && stats->mem_leaked) st->st.needs_reboot = true;     /* its memory is lost until the device restarts */
    ESP_LOGI(TAG, "%s ended with %d after %u ms", em.names_found[page], result, (unsigned)st->st.last_runtime_ms);
    st->st.running = false;   /* last: xip_loader_stop() may erase the slot once this is clear */
}

/* ---- public API ---- */
/* The name (manifest key 1, or "@<hex offset>") and the version (key 8, 0 without one) of the image at `offset`. */
static void ident_of(uint32_t offset, const xip_dmod_header *h, char *out, size_t cap, uint32_t *version)
{
    snprintf(out, cap, "@%x", (unsigned)offset);
    *version = 0;
    if (!h->manifest_size || h->manifest_size > 4096 || h->manifest_offset < XIP_DMOD_HEADER_SIZE || (h->manifest_offset & 3) ||
        h->manifest_offset > h->image_size || h->manifest_size > h->image_size - h->manifest_offset) return;
    uint8_t *m = malloc(h->manifest_size);
    if (!m) return;
    const uint8_t *text;
    size_t n;
    if (esp_partition_read(em.part, offset + h->manifest_offset, m, h->manifest_size) == ESP_OK) {
        int64_t v;
        if (xip_cbor_get_str(m, h->manifest_size, 1, XIP_CBOR_TEXT, &text, &n) == 0 && n > 0 && n < cap) {
            memcpy(out, text, n);
            out[n] = 0;
        }
        if (xip_cbor_get_int(m, h->manifest_size, 8, &v) == 0 && v >= 0 && v <= UINT32_MAX) *version = (uint32_t)v;
    }
    free(m);
}

/* The state of the page of an image that is now the active module: kept while it is the same image, started afresh otherwise. */
static void adopt(uint32_t page, uint32_t static_addr, uint32_t version)
{
    xip_slot_state_t *st = &em.slots[page];
    if (st->st.running || st->starting || st->inst) { st->st.version = version; return; }
    if (st->st.installed && st->static_addr == static_addr && st->st.version == version) return;
    free(st->request);
    memset(st, 0, sizeof(*st));
    st->static_addr = static_addr;
    st->st.last_result = INT32_MIN;
    st->st.installed = true;
    st->st.version = version;
}

/* Look at every page of the partition: a page that starts with an image header holds an image, which spans the pages its size says.
 * xip_select() picks the module of each name (the highest version); the others are listed as shadowed. The page an image sits on is
 * of no importance to the chip (docs/abi.md, "Versions and pages"). */
static int scan(void)
{
    xip_candidate_t *cand = calloc(em.capacity ? em.capacity : 1, sizeof(*cand));
    xip_choice_t *choice = calloc(em.capacity ? em.capacity : 1, sizeof(*choice));
    xip_slot_t *found = calloc(em.capacity ? em.capacity : 1, sizeof(*found));
    xip_shadow_t *shadows = calloc(em.capacity ? em.capacity : 1, sizeof(*shadows));
    uint16_t *tags = calloc(em.capacity ? em.capacity : 1, sizeof(*tags));
    uint32_t *sizes = calloc(em.capacity ? em.capacity : 1, sizeof(*sizes));
    if (!cand || !choice || !found || !shadows || !tags || !sizes) {
        free(cand); free(choice); free(found); free(shadows); free(tags); free(sizes);
        return XIP_LOADER_ERR_NOMEM;
    }
    size_t n = 0;
    for (uint32_t offset = 0; offset + XIP_SLOT_ALIGN <= em.part->size;) {
        xip_dmod_header h;
        if (esp_partition_read(em.part, offset, &h, sizeof(h)) != ESP_OK || h.magic != XIP_DMOD_MAGIC || h.image_size <= XIP_DMOD_HEADER_SIZE) {
            offset += XIP_SLOT_ALIGN;
            continue;
        }
        if (h.image_size > em.part->size - offset) { offset += XIP_SLOT_ALIGN; continue; }
        const uint32_t size = ((h.image_size + XIP_SLOT_ALIGN - 1) / XIP_SLOT_ALIGN) * XIP_SLOT_ALIGN;
        if (size > em.part->size - offset) {
            ESP_LOGW(TAG, "image at 0x%x does not fit its room", (unsigned)offset);
            offset += XIP_SLOT_ALIGN;
            continue;
        }
        xip_slot_rules_t rules = rules_for(offset, size);
        xip_dmod_result_t check = xip_dmod_check(&h, &rules);
        if (check != EMD_OK) {
            ESP_LOGW(TAG, "image at 0x%x rejected: %s", (unsigned)offset, xip_dmod_reason(check));
            offset += size;
            continue;
        }
        xip_candidate_t *c = &cand[n];
        c->page = offset / XIP_SLOT_ALIGN;
        c->static_addr = h.static_addr;
        c->window_slots = XIP_FIXED ? size / XIP_SLOT_ALIGN : 1;
        ident_of(offset, &h, c->name, sizeof(c->name), &c->version);
        tags[n] = h.tag;
        sizes[n] = size;
        n++;
        offset += size;
    }
    xip_select(cand, n, choice);

    size_t nf = 0, ns = 0;
    for (size_t i = 0; i < n; i++) {
        const uint32_t page = cand[i].page;
        char *name = em.names_found[page];
        if (!em.slots[page].st.running && !em.slots[page].starting) memcpy(name, cand[i].name, sizeof(cand[i].name));
        const xip_candidate_t *by = choice[i].by >= 0 ? &cand[choice[i].by] : NULL;
        switch (choice[i].role) {
        case XIP_ROLE_ACTIVE:
            found[nf++] = (xip_slot_t){.name = name, .tag = tags[i], .offset = page * XIP_SLOT_ALIGN, .size = sizes[i], .version = cand[i].version};
            break;
        case XIP_ROLE_SHADOWED:
        case XIP_ROLE_DUPLICATE:
            if (choice[choice[i].by].role == XIP_ROLE_ACTIVE)
                shadows[ns++] = (xip_shadow_t){.name = name, .tag = tags[i], .offset = page * XIP_SLOT_ALIGN, .version = cand[i].version,
                                               .shadowed_by = by->page * XIP_SLOT_ALIGN, .duplicate = choice[i].role == XIP_ROLE_DUPLICATE};
            if (choice[i].role == XIP_ROLE_DUPLICATE)
                ESP_LOGW(TAG, "duplicate of %s version %u: image at 0x%x is ignored", name, (unsigned)cand[i].version, (unsigned)(page * XIP_SLOT_ALIGN));
            break;
        case XIP_ROLE_CELL_CONFLICT:
            ESP_LOGW(TAG, "module slot range of %s is held by %s: image at 0x%x is ignored", name, by->name, (unsigned)(page * XIP_SLOT_ALIGN));
            break;
        }
    }
    for (size_t i = 0; i < n; i++)
        if (choice[i].role == XIP_ROLE_ACTIVE) adopt(cand[i].page, cand[i].static_addr, cand[i].version);
    /* a page that holds no image any more: its state goes, unless a module still runs from it */
    for (size_t p = 0; p < em.capacity; p++) {
        bool present = false;
        for (size_t i = 0; i < n; i++) present |= cand[i].page == p;
        xip_slot_state_t *st = &em.slots[p];
        if (!present && !st->st.running && !st->starting && !st->inst && st->st.installed) { free(st->request); memset(st, 0, sizeof(*st)); }
    }
    xSemaphoreTake(em.lock, portMAX_DELAY);
    memcpy(em.slots_found, found, nf * sizeof(*found));
    memcpy(em.shadows, shadows, ns * sizeof(*shadows));
    em.found = nf;
    em.shadow_found = ns;
    xSemaphoreGive(em.lock);
    free(cand); free(choice); free(found); free(shadows); free(tags); free(sizes);
    return (int)nf;
}

int xip_loader_rescan(void)
{
    if (!em.inited) return XIP_LOADER_ERR_NOT_RUNNING;
    int rc = scan();
    if (rc >= 0) {
        ESP_LOGI(TAG, "rescan: %d modules, %u shadowed", rc, (unsigned)em.shadow_found);
        for (size_t i = 0; i < em.shadow_found; i++)
            ESP_LOGI(TAG, "  %s version %u at 0x%x is %s 0x%x", em.shadows[i].name, (unsigned)em.shadows[i].version, (unsigned)em.shadows[i].offset,
                     em.shadows[i].duplicate ? "a duplicate of" : "shadowed by", (unsigned)em.shadows[i].shadowed_by);
    }
    return rc;
}

size_t xip_loader_shadow_count(void) { return em.inited ? em.shadow_found : 0; }

const xip_shadow_t *xip_loader_shadow(int index)
{
    return em.inited && index >= 0 && (size_t)index < em.shadow_found ? &em.shadows[index] : NULL;
}

esp_err_t xip_loader_init(const xip_loader_config_t *cfg)
{
    if (em.inited) return ESP_OK;
    if (!cfg || !cfg->partition) return ESP_ERR_INVALID_ARG;
    em.part = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_ANY, cfg->partition);
    if (!em.part) { ESP_LOGE(TAG, "partition \"%s\" not found", cfg->partition); return ESP_ERR_NOT_FOUND; }
    em.capacity = em.part->size / XIP_SLOT_ALIGN;
    em.slots = calloc(em.capacity ? em.capacity : 1, sizeof(*em.slots));
    em.slots_found = calloc(em.capacity ? em.capacity : 1, sizeof(*em.slots_found));
    em.names_found = calloc(em.capacity ? em.capacity : 1, sizeof(*em.names_found));
    em.shadows = calloc(em.capacity ? em.capacity : 1, sizeof(*em.shadows));
    em.lock = xSemaphoreCreateMutex();
    if (!em.slots || !em.slots_found || !em.names_found || !em.shadows || !em.lock) return ESP_ERR_NO_MEM;
    em.cfg = *cfg;
    if (!em.cfg.default_stack_bytes) em.cfg.default_stack_bytes = 8192;
    xip_module_ram_init();
    if (scan() < 0) return ESP_ERR_NO_MEM;
    em.inited = true;
    ESP_LOGI(TAG, "%u modules in \"%s\" (0x%x bytes at 0x%x)", (unsigned)em.found, cfg->partition,
             (unsigned)em.part->size, (unsigned)em.part->address);
    for (size_t i = 0; i < em.found; i++)
        ESP_LOGI(TAG, "  %s: component %u version %u at 0x%x", em.slots_found[i].name, (unsigned)em.slots_found[i].tag,
                 (unsigned)em.slots_found[i].version, (unsigned)em.slots_found[i].offset);
    for (size_t i = 0; i < em.shadow_found; i++)
        ESP_LOGI(TAG, "  %s version %u at 0x%x is %s 0x%x", em.shadows[i].name, (unsigned)em.shadows[i].version, (unsigned)em.shadows[i].offset,
                 em.shadows[i].duplicate ? "a duplicate of" : "shadowed by", (unsigned)em.shadows[i].shadowed_by);
    return ESP_OK;
}

int xip_loader_find(const char *name)
{
    if (!em.inited || !name) return -1;
    for (size_t i = 0; i < em.found; i++) if (!strcmp(em.slots_found[i].name, name)) return (int)i;
    return -1;
}

size_t xip_loader_slot_count(void) { return em.inited ? em.found : 0; }

const xip_slot_t *xip_loader_slot(int slot)
{
    return em.inited && slot >= 0 && (size_t)slot < em.found ? &em.slots_found[slot] : NULL;
}

/* An integer of the manifest (key 9: the datagram port the module is bound to; key 10: its stream port, which listens on TCP). 0: none. */
/* Manifest key 11: the names of the components the module hosts (an array of at most XIP_COMPONENTS_MAX texts of at most 15 bytes). Others are ignored. */
static void manifest_components(int slot, const xip_dmod_header *h, char names[XIP_COMPONENTS_MAX][16])
{
    memset(names, 0, XIP_COMPONENTS_MAX * 16);
    if (!h->manifest_size || h->manifest_size > 4096) return;
    uint8_t *m = malloc(h->manifest_size);
    if (!m) return;
    xip_cbor_r v;
    xip_cbor_item it;
    if (esp_partition_read(em.part, em.slots_found[slot].offset + h->manifest_offset, m, h->manifest_size) == ESP_OK &&
        xip_cbor_get(m, h->manifest_size, 11, &v) == 0 && xip_cbor_next(&v, &it) == 0 && it.type == XIP_CBOR_ARRAY) {
        for (size_t i = 0, k = 0; i < it.n && k < XIP_COMPONENTS_MAX; i++) {
            xip_cbor_item t;
            if (xip_cbor_next(&v, &t) || t.type != XIP_CBOR_TEXT) break;
            if (t.n && t.n < 16) { memcpy(names[k], t.data, t.n); k++; }
        }
    }
    free(m);
}

static uint32_t manifest_port(int slot, const xip_dmod_header *h, uint64_t key)
{
    if (!h->manifest_size || h->manifest_size > 4096) return 0;
    uint8_t *m = malloc(h->manifest_size);
    if (!m) return 0;
    int64_t port = 0;
    if (esp_partition_read(em.part, em.slots_found[slot].offset + h->manifest_offset, m, h->manifest_size) != ESP_OK ||
        xip_cbor_get_int(m, h->manifest_size, key, &port) != 0 || port < 0 || port > UINT32_MAX)
        port = 0;
    free(m);
    return (uint32_t)port;
}

int xip_loader_start(int slot, const uint8_t *request, size_t request_len)
{
    if (!xip_loader_slot(slot)) return XIP_LOADER_ERR_SLOT;
    if (request_len && !request) return XIP_ERR_ARG;
    xip_slot_state_t *st = XIP_SLOT_STATE(slot);
    if (st->st.running || st->starting) return XIP_LOADER_ERR_RUNNING;

    xip_dmod_header h;
    xip_slot_rules_t rules = xip_rules(slot);
    st->st.last_check = xip_read_header(slot, &h) == ESP_OK ? xip_dmod_check(&h, &rules) : EMD_BAD_MAGIC;
    st->st.installed = st->st.last_check != EMD_BAD_MAGIC;
    if (st->st.last_check != EMD_OK) {
        ESP_LOGW(TAG, "%s: image rejected: %s", em.slots_found[slot].name,
                 xip_dmod_reason(st->st.last_check));
        return XIP_LOADER_ERR_IMAGE;
    }
    uint8_t *copy = NULL;
    if (request_len) {
        copy = malloc(request_len);
        if (!copy) return XIP_LOADER_ERR_NOMEM;
        memcpy(copy, request, request_len);
    }
    xSemaphoreTake(em.lock, portMAX_DELAY);
    bool conflict = st->st.running || st->starting;
    for (size_t p = 0; p < em.capacity; p++) {      /* the cell is held by a running or starting instance, whichever version it is */
        const xip_slot_state_t *o = &em.slots[p];
        if (p != XIP_SLOT_PAGE(slot) && xip_slots_overlap(o->static_addr, o->window_slots, h.static_addr,
                XIP_FIXED ? (h.image_size + XIP_SLOT_ALIGN - 1) / XIP_SLOT_ALIGN : 1) && (o->st.running || o->starting || o->inst)) conflict = true;
    }
    if (conflict) {
        xSemaphoreGive(em.lock);
        free(copy);
        return XIP_LOADER_ERR_RUNNING;
    }
    st->starting = true;
    st->static_addr = h.static_addr;
    st->window_slots = XIP_FIXED ? (h.image_size + XIP_SLOT_ALIGN - 1) / XIP_SLOT_ALIGN : 1;
    xSemaphoreGive(em.lock);
    // The claim excludes every running or starting instance before any bytes
    // are cleared. PIC relocation never changes the cell's identity.
    if (!xip_module_ram_clear(h.static_addr)) {
        xSemaphoreTake(em.lock, portMAX_DELAY); st->starting = false; xSemaphoreGive(em.lock);
        free(copy);
        return XIP_LOADER_ERR_IMAGE;
    }
    free(st->request);
    st->request = copy;
    st->request_len = request_len;

    const void *base;
    if (map_image(slot, &h, st, &base) != ESP_OK) {
        xSemaphoreTake(em.lock, portMAX_DELAY); st->starting = false; xSemaphoreGive(em.lock);
        return XIP_LOADER_ERR_MAP;
    }
    const xip_instance_config_t ic = {.handler = (xip_handle_fn)((const uint8_t *)base + h.entry_offset),
                                         .state_bytes = h.state_bytes, .done = instance_done,
                                         .done_ctx = (void *)(intptr_t)XIP_SLOT_PAGE(slot), .component = h.tag,
                                         .port = manifest_port(slot, &h, 9), .stream = manifest_port(slot, &h, 10)};
    ESP_LOGI(TAG, "%s: handler %p, image %u bytes, state %u", em.slots_found[slot].name, (void *)ic.handler,
             (unsigned)h.image_size, (unsigned)h.state_bytes);
    xip_instance_config_t config = ic;
    manifest_components(slot, &h, config.components);
    xip_instance_t *inst = xip_instance_create(em.slots_found[slot].name, &config);
    if (!inst) {
        unmap_image(st);
        xSemaphoreTake(em.lock, portMAX_DELAY); st->starting = false; xSemaphoreGive(em.lock);
        return XIP_LOADER_ERR_NOMEM;
    }
    xSemaphoreTake(em.lock, portMAX_DELAY);
    st->inst = inst;
    st->starting = false;
    st->st.running = true;
    xSemaphoreGive(em.lock);
    st->started_us = esp_timer_get_time();
    int rc = xip_instance_start(inst, st->request, st->request_len);
    if (rc) {   /* the instance never ran: release it by stopping (the worker calls instance_done) */
        (void)xip_instance_stop(inst);
        return XIP_LOADER_ERR_NOMEM;
    }
    return 0;
}

int xip_loader_stop(int slot, uint32_t timeout_ms)
{
    if (!xip_loader_slot(slot)) return XIP_LOADER_ERR_SLOT;
    xip_slot_state_t *st = XIP_SLOT_STATE(slot);
    if (!st->st.running) return 0;
    xSemaphoreTake(em.lock, portMAX_DELAY);
    xip_instance_t *inst = st->inst;      /* NULL (already ending) is ignored */
    xSemaphoreGive(em.lock);
    if (inst) (void)xip_instance_stop(inst);
    if (timeout_ms == 0) return 0;      /* queued: the module ends in the background */
    TickType_t deadline = xTaskGetTickCount() + pdMS_TO_TICKS(timeout_ms);
    while (st->st.running) {
        if ((int32_t)(xTaskGetTickCount() - deadline) >= 0) return XIP_LOADER_ERR_TIMEOUT;
        vTaskDelay(1);
    }
    return 0;
}

static xip_instance_t *running_instance(int slot)
{
    xip_slot_state_t *st = XIP_SLOT_STATE(slot);
    xSemaphoreTake(em.lock, portMAX_DELAY);
    xip_instance_t *inst = st->st.running ? st->inst : NULL;
    xSemaphoreGive(em.lock);
    return inst;
}

int xip_loader_post(int slot, int component, int method, const uint8_t *data, size_t len)
{
    if (!xip_loader_slot(slot)) return XIP_LOADER_ERR_SLOT;
    xip_instance_t *inst = running_instance(slot);
    return inst ? xip_instance_post(inst, component, method, data, len) : XIP_LOADER_ERR_NOT_RUNNING;
}

int xip_loader_request(int slot, int component, int method, const uint8_t *data, size_t len,
                          uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms)
{
    if (!xip_loader_slot(slot)) return XIP_LOADER_ERR_SLOT;
    xip_instance_t *inst = running_instance(slot);
    return inst ? xip_instance_request(inst, component, method, data, len, response, capacity, response_len, timeout_ms)
                : XIP_LOADER_ERR_NOT_RUNNING;
}

void xip_loader_status(int slot, xip_slot_status_t *out)
{
    if (!out) return;
    if (!xip_loader_slot(slot)) { memset(out, 0, sizeof(*out)); return; }
    *out = XIP_SLOT_STATE(slot)->st;
}

/* ---- by name, for the ABI's module calls (the host's hooks) ---- */
static int find_named(const uint8_t *name, size_t n)
{
    char c[16];
    if (!n || n >= sizeof(c)) return -1;
    memcpy(c, name, n);
    c[n] = 0;
    return xip_loader_find(c);
}

int xip_loader_start_named(const uint8_t *name, size_t n, const uint8_t *request, size_t request_len)
{
    int slot = find_named(name, n);
    return slot < 0 ? XIP_ERR_NOTFOUND : xip_loader_start(slot, request, request_len);
}

int xip_loader_stop_named(const uint8_t *name, size_t n)
{
    int slot = find_named(name, n);
    return slot < 0 ? XIP_ERR_NOTFOUND : xip_loader_stop(slot, 0);
}

int xip_loader_manifest(int slot, uint8_t *out, size_t capacity)
{
    if (!xip_loader_slot(slot)) return XIP_LOADER_ERR_SLOT;
    xip_dmod_header h;
    if (xip_read_header(slot, &h) != ESP_OK || h.magic != XIP_DMOD_MAGIC) return XIP_LOADER_ERR_IMAGE;
    xip_slot_rules_t rules = xip_rules(slot);
    if (xip_dmod_check(&h, &rules) != EMD_OK) return XIP_LOADER_ERR_IMAGE;     /* the offsets are only trusted after the check */
    if (!h.manifest_size) return 0;
    if (h.manifest_size > capacity) return XIP_ERR_SIZE;
    if (esp_partition_read(em.part, em.slots_found[slot].offset + h.manifest_offset, out, h.manifest_size) != ESP_OK) return XIP_ERR_IO;
    return (int)h.manifest_size;
}

int xip_loader_autostart(void)
{
    enum { MAX_MANIFEST = 1024 };
    uint8_t *m = malloc(MAX_MANIFEST);
    int started = 0;
    if (!m) return 0;
    for (size_t i = 0; i < xip_loader_slot_count(); i++) {
        int n = xip_loader_manifest((int)i, m, MAX_MANIFEST);
        const uint8_t *request;
        size_t rn;
        ESP_LOGD(TAG, "autostart: slot %d manifest %d", (int)i, n);
        if (n <= 0 || xip_cbor_get_str(m, (size_t)n, 6, XIP_CBOR_TEXT, &request, &rn) != 0) continue;
        int rc = xip_loader_start((int)i, request, rn);
        ESP_LOGW(TAG, "autostart %s: %d", em.slots_found[i].name, rc);
        if (rc == 0) started++;
    }
    free(m);
    return started;
}

/* Start the module that hosts `component` (manifest key 11) if it is installed and not running: the runtime's get_component asks (the hook
 * `component_start`). 0: started or already running; XIP_ERR_NOTFOUND: no installed module names it; otherwise the start's error. The highest
 * version of the module's name runs, as for any start by name. */
int xip_loader_start_hosting(const char *component)
{
    enum { MAX_MANIFEST = 1024 };
    uint8_t *m = malloc(MAX_MANIFEST);
    if (!m) return XIP_ERR_NOMEM;
    int rc = XIP_ERR_NOTFOUND;
    for (size_t i = 0; i < xip_loader_slot_count() && rc == XIP_ERR_NOTFOUND; i++) {
        int n = xip_loader_manifest((int)i, m, MAX_MANIFEST);
        xip_cbor_r v;
        xip_cbor_item it;
        if (n <= 0 || xip_cbor_get(m, (size_t)n, 11, &v) != 0 || xip_cbor_next(&v, &it) != 0 || it.type != XIP_CBOR_ARRAY) continue;
        for (size_t k = 0; k < it.n; k++) {
            xip_cbor_item t;
            if (xip_cbor_next(&v, &t) || t.type != XIP_CBOR_TEXT) break;
            if (t.n != strlen(component) || memcmp(t.data, component, t.n)) continue;
            const char *name = em.slots_found[i].name;
            rc = xip_loader_start_named((const uint8_t *)name, strlen(name), NULL, 0);
            if (rc == XIP_LOADER_ERR_RUNNING) rc = 0;
            break;
        }
    }
    free(m);
    return rc;
}

int xip_loader_manifest_named(const uint8_t *name, size_t n, uint8_t *out, size_t capacity)
{
    int slot = find_named(name, n);
    return slot < 0 ? XIP_ERR_NOTFOUND : xip_loader_manifest(slot, out, capacity);
}

/* Start `version` of a module, a shadowed one included: it takes the place of the active image in the slot list (until the next rescan). */
int xip_loader_start_version(const uint8_t *name, size_t n, uint32_t version, const uint8_t *request, size_t request_len)
{
    int slot = find_named(name, n);
    if (slot < 0) return XIP_ERR_NOTFOUND;
    if (em.slots_found[slot].version == version) return xip_loader_start(slot, request, request_len);
    xSemaphoreTake(em.lock, portMAX_DELAY);
    int rc = XIP_ERR_NOTFOUND;
    for (size_t i = 0; i < em.shadow_found && rc == XIP_ERR_NOTFOUND; i++) {
        xip_shadow_t *sh = &em.shadows[i];
        if (sh->duplicate || sh->version != version || strcmp(sh->name, em.slots_found[slot].name)) continue;
        xip_slot_state_t *cur = XIP_SLOT_STATE(slot);
        if (cur->st.running || cur->starting || cur->inst) { rc = XIP_LOADER_ERR_RUNNING; break; }
        /* swap the roles of the two pages; both share the module's RAM cell */
        xip_slot_t *act = &em.slots_found[slot];
        const xip_slot_t old = *act;
        const uint32_t new_page = sh->offset / XIP_SLOT_ALIGN;
        act->offset = sh->offset;
        act->tag = sh->tag;
        act->version = sh->version;
        act->size = old.size;                  /* one page on Xtensa; a larger image is checked by xip_loader_start */
        *sh = (xip_shadow_t){.name = old.name, .tag = old.tag, .offset = old.offset, .version = old.version, .shadowed_by = sh->offset};
        rc = 0;
        for (size_t k = 0; k < em.shadow_found; k++)
            if (k != i && em.shadows[k].shadowed_by == old.offset) em.shadows[k].shadowed_by = act->offset;
        em.slots[new_page].st.installed = false;     /* adopt() below starts the page afresh */
    }
    xSemaphoreGive(em.lock);
    if (rc) return rc;
    {
        xip_dmod_header h;
        if (xip_read_header(slot, &h) != ESP_OK) return XIP_LOADER_ERR_IMAGE;
        em.slots_found[slot].size = ((h.image_size + XIP_SLOT_ALIGN - 1) / XIP_SLOT_ALIGN) * XIP_SLOT_ALIGN;
        adopt(XIP_SLOT_PAGE(slot), h.static_addr, version);
    }
    return xip_loader_start(slot, request, request_len);
}

/* CBOR: an array with a map per slot ({1: name, 2: tag, 3: installed, 4: running, 5: version, 6: last result, 7: runs,
 * 8: ms of the last run, 9: stack high-water mark, 10: result of the last image check, 11: reboot needed, 12: flash page of the image,
 * 13: the shadowed images of this module: an array of {1: page, 2: version, 3: duplicate}}; `Slot` of api/host.API.md) */
int xip_loader_modules_info(uint8_t *out, size_t cap)
{
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    const size_t count = xip_loader_slot_count();
    xip_cbor_array(&w, count);
    for (size_t i = 0; i < count; i++) {
        const xip_slot_t *slot = xip_loader_slot((int)i);
        xip_slot_status_t st;
        xip_loader_status((int)i, &st);
        size_t shadowed = 0;
        for (size_t k = 0; k < em.shadow_found; k++) shadowed += em.shadows[k].shadowed_by == slot->offset;
        xip_cbor_map(&w, 13);
        xip_cbor_uint(&w, 1); xip_cbor_text(&w, slot->name, strlen(slot->name));
        xip_cbor_uint(&w, 2); xip_cbor_uint(&w, slot->tag);
        xip_cbor_uint(&w, 3); xip_cbor_bool(&w, st.installed);
        xip_cbor_uint(&w, 4); xip_cbor_bool(&w, st.running);
        xip_cbor_uint(&w, 5); xip_cbor_uint(&w, st.version);
        xip_cbor_uint(&w, 6); xip_cbor_int(&w, st.last_result == INT32_MIN || st.last_result == XIP_RET_DONE ? 0 : st.last_result);   /* DONE is success */
        xip_cbor_uint(&w, 7); xip_cbor_uint(&w, st.runs);
        xip_cbor_uint(&w, 8); xip_cbor_uint(&w, st.last_runtime_ms);
        xip_cbor_uint(&w, 9); xip_cbor_uint(&w, st.stack_high_water_bytes);
        xip_cbor_uint(&w, 10); xip_cbor_int(&w, st.last_check);
        xip_cbor_uint(&w, 11); xip_cbor_bool(&w, st.needs_reboot);
        xip_cbor_uint(&w, 12); xip_cbor_uint(&w, slot->offset / XIP_SLOT_ALIGN);
        xip_cbor_uint(&w, 13); xip_cbor_array(&w, shadowed);
        for (size_t k = 0; k < em.shadow_found; k++) {
            const xip_shadow_t *sh = &em.shadows[k];
            if (sh->shadowed_by != slot->offset) continue;
            xip_cbor_map(&w, 3);
            xip_cbor_uint(&w, 1); xip_cbor_uint(&w, sh->offset / XIP_SLOT_ALIGN);
            xip_cbor_uint(&w, 2); xip_cbor_uint(&w, sh->version);
            xip_cbor_uint(&w, 3); xip_cbor_bool(&w, sh->duplicate);
        }
    }
    return xip_cbor_done(&w);
}
