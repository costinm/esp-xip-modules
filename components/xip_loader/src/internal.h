/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/semphr.h"
#include "esp_partition.h"
#include "xip_check.h"
#include "xip_loader.h"
#include "xip_runtime.h"

typedef struct { uint32_t code_vaddr, data_vaddr, mapped; } fixed_map_t;

/* The state of a module is kept per flash page (the page is the identity that does not move when the slot list does). */
typedef struct {
    uint32_t window_slots;      /* claimed before mapping, including while starting */
    uint32_t static_addr;       /* identity from the image, never its discovery index */
    bool starting;             /* claims the cell before an instance is created */
    xip_instance_t *inst;      /* NULL when not running */
#if CONFIG_IDF_TARGET_ESP32 || CONFIG_IDF_TARGET_ESP32S3 || CONFIG_IDF_TARGET_ESP32C6
    fixed_map_t fixed;
#else
    esp_partition_mmap_handle_t mmap;
#endif
    uint8_t *request;             /* last start request, kept for a restart */
    size_t request_len;
    xip_slot_status_t st;
    int64_t started_us;
} xip_slot_state_t;

typedef struct {
    xip_loader_config_t cfg;
    xip_slot_t *slots_found;      /* the scan: the active images in page order; names point into names_found */
    xip_shadow_t *shadows;        /* the scan: images not picked */
    size_t shadow_found;
    char (*names_found)[24];      /* per page */
    size_t found, capacity;      /* modules found, and the pages the partition has */
    const esp_partition_t *part;
    xip_extent_t runtimes[XIP_LOADER_RUNTIMES_MAX];   /* the scan: the runtime images, in address order */
    size_t runtime_found;
    xip_extent_t *extents;        /* the scan: every image (runtimes, active and shadowed modules) in address order; capacity + runtimes entries */
    size_t extent_found;
    xip_slot_state_t *slots;      /* per page (not per slot index) */
    SemaphoreHandle_t lock;       /* guards xip_slot_state_t.inst: a stop or post must not meet the exit of the task */
    bool inited;
    bool safe;               /* crash loop with nothing to fall back to: autostart only cfg.safe_modules */
} xip_loader_t;

extern xip_loader_t em;

/* The page of a slot index, and its state. */
#define XIP_SLOT_PAGE(slot) (em.slots_found[slot].offset / XIP_SLOT_ALIGN)
#define XIP_SLOT_STATE(slot) (&em.slots[XIP_SLOT_PAGE(slot)])

/* Shared by loader.c and upload.c */
xip_slot_rules_t xip_rules(int slot);
esp_err_t xip_read_header(int slot, xip_dmod_header *h);
