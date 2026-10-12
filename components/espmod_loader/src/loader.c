/* SPDX-License-Identifier: Apache-2.0 */
/* Map a module image from flash, run it on its own task, stop it, report on it.
 *
 * Where the code runs depends on the chip:
 *  - ESP32 and ESP32-S3 (Xtensa): the toolchain cannot make position independent code, so each slot
 *    is linked for one fixed address (code_base + slot offset) and the loader maps the flash pages
 *    at exactly that address in both the instruction and the data alias (literals are data loads).
 *    That needs the HAL MMU calls: esp_partition_mmap() never lets the caller choose the address.
 *  - ESP32-C6 (RISC-V): the image is position independent; esp_partition_mmap() is enough.
 * The fixed-address part is the proven code of DMesh's module loader (docs/architecture.md). */
#include <stdlib.h>
#include <string.h>
#include "esp_log.h"
#include "espmod_cbor.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "internal.h"
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

static const char *TAG = "espmod-loader";
em_loader_t em;

#if CONFIG_IDF_TARGET_ESP32S3
#define EM_FIXED 1
#define EM_CODE_BASE 0x43000000u
#define EM_DATA_BASE 0x3d000000u
#define EM_WINDOW_MAX 0x1000000u
#elif CONFIG_IDF_TARGET_ESP32
#define EM_FIXED 1
/* Above Main's own IROM (0x400d0000..) and DROM. */
#define EM_CODE_BASE 0x40300000u
#define EM_DATA_BASE 0x3f700000u
#define EM_WINDOW_MAX 0x100000u
#else
#define EM_FIXED 0
#define EM_CODE_BASE 0u
#define EM_DATA_BASE 0u
#define EM_WINDOW_MAX 0xffffffffu
#endif

#define HOST_FEATURES (ESPMOD_FEAT_SETTINGS | ESPMOD_FEAT_SPI | ESPMOD_FEAT_SPI_READY | ESPMOD_FEAT_I2C | \
                       ESPMOD_FEAT_ADC | ESPMOD_FEAT_RGBLED | ESPMOD_FEAT_SERVICE)

espmod_slot_rules_t em_rules(int slot)
{
    const espmod_slot_t *s = &em.cfg.slots[slot];
    return (espmod_slot_rules_t){.tag = s->tag, .slot_offset = s->offset, .slot_size = s->size,
                                 .host_features = HOST_FEATURES, .max_stack_bytes = CONFIG_ESPMOD_WORKER_STACK,
                                 .fixed_vma = EM_FIXED, .code_base = EM_CODE_BASE, .data_base = EM_DATA_BASE};
}

esp_err_t em_read_header(int slot, espmod_dmod_header *h)
{
    return esp_partition_read(em.part, em.cfg.slots[slot].offset, h, sizeof(*h));
}

#if EM_FIXED
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

static esp_err_t map_fixed(int slot, const espmod_dmod_header *h, fixed_map_t *m)
{
    const uint32_t id = mmu_id(), page = mmu_hal_pages_to_bytes(id, 1);
    const uint32_t paddr = em.part->address + em.cfg.slots[slot].offset;
    const uint32_t rounded = ((h->image_size + page - 1) / page) * page;
    if (page == 0 || paddr % page || h->code_vma % page || h->data_vma % page || rounded > em.cfg.slots[slot].size)
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
static esp_err_t map_image(int slot, const espmod_dmod_header *h, em_slot_state_t *st, const void **base)
{
    *base = NULL;
#if EM_FIXED
    if (map_fixed(slot, h, &st->fixed) != ESP_OK) return ESP_FAIL;
    *base = (const void *)(uintptr_t)st->fixed.code_vaddr;
    return ESP_OK;
#else
    if (esp_partition_mmap(em.part, em.cfg.slots[slot].offset, h->image_size, ESP_PARTITION_MMAP_INST, base,
                           &st->mmap) != ESP_OK) { *base = NULL; return ESP_FAIL; }
    return ESP_OK;
#endif
}

static void unmap_image(em_slot_state_t *st)
{
#if EM_FIXED
    unmap_fixed(&st->fixed);
#else
    esp_partition_munmap(st->mmap);
#endif
}

/* On the worker, after the instance is gone: nothing runs from the slot any more. */
static void instance_done(void *ctx, int result, const espmod_stats_t *stats)
{
    const int slot = (int)(intptr_t)ctx;
    em_slot_state_t *st = &em.slots[slot];
    unmap_image(st);
    xSemaphoreTake(em.lock, portMAX_DELAY);
    st->inst = NULL;                         /* from here no stop or post can reach it */
    xSemaphoreGive(em.lock);
    st->st.stack_high_water_bytes = espmod_host_stack_high_water();
    st->st.last_result = result;
    st->st.last_runtime_ms = (uint32_t)((esp_timer_get_time() - st->started_us) / 1000);
    st->st.runs++;
    (void)stats;
    ESP_LOGI(TAG, "%s ended with %d after %u ms", em.cfg.slots[slot].name, result, (unsigned)st->st.last_runtime_ms);
    st->st.running = false;   /* last: espmod_loader_stop() may erase the slot once this is clear */
}

/* ---- public API ---- */
esp_err_t espmod_loader_init(const espmod_loader_config_t *cfg)
{
    if (em.inited) return ESP_OK;
    if (!cfg || !cfg->slots || !cfg->slot_count || !cfg->partition) return ESP_ERR_INVALID_ARG;
    em.part = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_ANY, cfg->partition);
    if (!em.part) { ESP_LOGE(TAG, "partition \"%s\" not found", cfg->partition); return ESP_ERR_NOT_FOUND; }
    for (size_t i = 0; i < cfg->slot_count; i++) {
        const espmod_slot_t *s = &cfg->slots[i];
        if (!s->name || !s->size || s->offset % ESPMOD_SLOT_ALIGN || s->size % ESPMOD_SLOT_ALIGN ||
            s->offset > em.part->size || s->size > em.part->size - s->offset ||
            (EM_FIXED && (uint64_t)s->offset + s->size > EM_WINDOW_MAX)) {
            ESP_LOGE(TAG, "slot %s does not fit", s->name ? s->name : "?");
            return ESP_ERR_INVALID_ARG;
        }
        for (size_t j = 0; j < i; j++) {
            const espmod_slot_t *o = &cfg->slots[j];
            if (s->offset < o->offset + o->size && o->offset < s->offset + s->size) return ESP_ERR_INVALID_ARG;
        }
    }
    em.slots = calloc(cfg->slot_count, sizeof(*em.slots));
    em.lock = xSemaphoreCreateMutex();
    if (!em.slots || !em.lock) return ESP_ERR_NO_MEM;
    em.cfg = *cfg;
    if (!em.cfg.default_stack_bytes) em.cfg.default_stack_bytes = 8192;
    for (size_t i = 0; i < cfg->slot_count; i++) {
        em.slots[i].st.last_result = INT32_MIN;
        espmod_dmod_header h;
        em.slots[i].st.installed = em_read_header((int)i, &h) == ESP_OK && h.magic == ESPMOD_DMOD_MAGIC;
    }
    em.inited = true;
    ESP_LOGI(TAG, "%u slots in \"%s\" (0x%x bytes at 0x%x)", (unsigned)cfg->slot_count, cfg->partition,
             (unsigned)em.part->size, (unsigned)em.part->address);
    return ESP_OK;
}

int espmod_loader_find(const char *name)
{
    if (!em.inited || !name) return -1;
    for (size_t i = 0; i < em.cfg.slot_count; i++) if (!strcmp(em.cfg.slots[i].name, name)) return (int)i;
    return -1;
}

size_t espmod_loader_slot_count(void) { return em.inited ? em.cfg.slot_count : 0; }

const espmod_slot_t *espmod_loader_slot(int slot)
{
    return em.inited && slot >= 0 && (size_t)slot < em.cfg.slot_count ? &em.cfg.slots[slot] : NULL;
}

int espmod_loader_start(int slot, const uint8_t *request, size_t request_len)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    if (request_len && !request) return ESPMOD_ERR_ARG;
    em_slot_state_t *st = &em.slots[slot];
    if (st->st.running) return ESPMOD_LOADER_ERR_RUNNING;

    espmod_dmod_header h;
    espmod_slot_rules_t rules = em_rules(slot);
    st->st.last_check = em_read_header(slot, &h) == ESP_OK ? espmod_dmod_check(&h, &rules) : EMD_BAD_MAGIC;
    st->st.installed = st->st.last_check != EMD_BAD_MAGIC;
    if (st->st.last_check != EMD_OK) {
        ESP_LOGW(TAG, "%s: image rejected: %s", em.cfg.slots[slot].name,
                 espmod_dmod_reason(st->st.last_check));
        return ESPMOD_LOADER_ERR_IMAGE;
    }
    uint8_t *copy = NULL;
    if (request_len) {
        copy = malloc(request_len);
        if (!copy) return ESPMOD_LOADER_ERR_NOMEM;
        memcpy(copy, request, request_len);
    }
    free(st->request);
    st->request = copy;
    st->request_len = request_len;

    const void *base;
    if (map_image(slot, &h, st, &base) != ESP_OK) return ESPMOD_LOADER_ERR_MAP;
    const espmod_instance_config_t ic = {.handler = (espmod_handle_fn)((const uint8_t *)base + h.entry_offset),
                                         .state_bytes = h.state_bytes, .done = instance_done,
                                         .done_ctx = (void *)(intptr_t)slot, .component = em.cfg.slots[slot].tag};
    ESP_LOGI(TAG, "%s: handler %p, image %u bytes, state %u", em.cfg.slots[slot].name, (void *)ic.handler,
             (unsigned)h.image_size, (unsigned)h.state_bytes);
    espmod_instance_t *inst = espmod_instance_create(em.cfg.slots[slot].name, &ic);
    if (!inst) { unmap_image(st); return ESPMOD_LOADER_ERR_NOMEM; }
    xSemaphoreTake(em.lock, portMAX_DELAY);
    st->inst = inst;
    xSemaphoreGive(em.lock);
    st->started_us = esp_timer_get_time();
    st->st.running = true;
    int rc = espmod_instance_start(inst, st->request, st->request_len);
    if (rc) {   /* the instance never ran: release it by stopping (the worker calls instance_done) */
        (void)espmod_instance_stop(inst);
        return ESPMOD_LOADER_ERR_NOMEM;
    }
    return 0;
}

int espmod_loader_stop(int slot, uint32_t timeout_ms)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    em_slot_state_t *st = &em.slots[slot];
    if (!st->st.running) return 0;
    xSemaphoreTake(em.lock, portMAX_DELAY);
    espmod_instance_t *inst = st->inst;      /* NULL (already ending) is ignored */
    xSemaphoreGive(em.lock);
    if (inst) (void)espmod_instance_stop(inst);
    if (timeout_ms == 0) return 0;      /* queued: the module ends in the background */
    TickType_t deadline = xTaskGetTickCount() + pdMS_TO_TICKS(timeout_ms);
    while (st->st.running) {
        if ((int32_t)(xTaskGetTickCount() - deadline) >= 0) return ESPMOD_LOADER_ERR_TIMEOUT;
        vTaskDelay(1);
    }
    return 0;
}

static espmod_instance_t *running_instance(int slot)
{
    em_slot_state_t *st = &em.slots[slot];
    xSemaphoreTake(em.lock, portMAX_DELAY);
    espmod_instance_t *inst = st->st.running ? st->inst : NULL;
    xSemaphoreGive(em.lock);
    return inst;
}

int espmod_loader_post(int slot, int component, int method, const uint8_t *data, size_t len)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    espmod_instance_t *inst = running_instance(slot);
    return inst ? espmod_instance_post(inst, component, method, data, len) : ESPMOD_LOADER_ERR_NOT_RUNNING;
}

int espmod_loader_request(int slot, int component, int method, const uint8_t *data, size_t len,
                          uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms)
{
    if (!espmod_loader_slot(slot)) return ESPMOD_LOADER_ERR_SLOT;
    espmod_instance_t *inst = running_instance(slot);
    return inst ? espmod_instance_request(inst, component, method, data, len, response, capacity, response_len, timeout_ms)
                : ESPMOD_LOADER_ERR_NOT_RUNNING;
}

void espmod_loader_status(int slot, espmod_slot_status_t *out)
{
    if (!out) return;
    if (!espmod_loader_slot(slot)) { memset(out, 0, sizeof(*out)); return; }
    *out = em.slots[slot].st;
}

/* ---- by name, for the ABI's module calls (the host's hooks) ---- */
static int find_named(const uint8_t *name, size_t n)
{
    char c[16];
    if (!n || n >= sizeof(c)) return -1;
    memcpy(c, name, n);
    c[n] = 0;
    return espmod_loader_find(c);
}

int espmod_loader_start_named(const uint8_t *name, size_t n, const uint8_t *request, size_t request_len)
{
    int slot = find_named(name, n);
    return slot < 0 ? ESPMOD_ERR_NOTFOUND : espmod_loader_start(slot, request, request_len);
}

int espmod_loader_stop_named(const uint8_t *name, size_t n)
{
    int slot = find_named(name, n);
    return slot < 0 ? ESPMOD_ERR_NOTFOUND : espmod_loader_stop(slot, 0);
}

/* CBOR: an array with a map per slot ({1: name, 2: tag, 3: installed, 4: running, 5: version, 6: last result, 7: runs,
 * 8: ms of the last run, 9: stack high-water mark, 10: result of the last image check}; `slot` of mod_console/API.md) */
int espmod_loader_modules_info(uint8_t *out, size_t cap)
{
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    const size_t count = espmod_loader_slot_count();
    em_cbor_array(&w, count);
    for (size_t i = 0; i < count; i++) {
        const espmod_slot_t *slot = espmod_loader_slot((int)i);
        espmod_slot_status_t st;
        espmod_loader_status((int)i, &st);
        em_cbor_map(&w, 10);
        em_cbor_uint(&w, 1); em_cbor_text(&w, slot->name, strlen(slot->name));
        em_cbor_uint(&w, 2); em_cbor_uint(&w, slot->tag);
        em_cbor_uint(&w, 3); em_cbor_bool(&w, st.installed);
        em_cbor_uint(&w, 4); em_cbor_bool(&w, st.running);
        em_cbor_uint(&w, 5); em_cbor_uint(&w, st.version);
        em_cbor_uint(&w, 6); em_cbor_int(&w, st.last_result == INT32_MIN || st.last_result == ESPMOD_RET_DONE ? 0 : st.last_result);   /* DONE is success */
        em_cbor_uint(&w, 7); em_cbor_uint(&w, st.runs);
        em_cbor_uint(&w, 8); em_cbor_uint(&w, st.last_runtime_ms);
        em_cbor_uint(&w, 9); em_cbor_uint(&w, st.stack_high_water_bytes);
        em_cbor_uint(&w, 10); em_cbor_int(&w, st.last_check);
    }
    return em_cbor_done(&w);
}
