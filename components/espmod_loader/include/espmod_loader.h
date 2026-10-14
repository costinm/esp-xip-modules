/* SPDX-License-Identifier: Apache-2.0 */
/* Runs module images from flash. A module is not a task: its image stays mapped
 * while the host's worker calls its handler.
 *
 * The application passes a slot table (which module lives where). The loader maps an image, runs it on the host and
 * stops it; it has no network, crypto, or image installer. It knows no module by name.
 * Mapping, tasks and the host table are the part that depends on the chip; the checks before a
 * module is mapped are in espmod_check.h. */
#pragma once

#include <stdbool.h>
#include "esp_err.h"
#include "espmod.h"

#ifdef __cplusplus
extern "C" {
#endif

#define ESPMOD_SLOT_ALIGN 0x10000u   /* the flash MMU page: slots start and end on it */

typedef struct {
    const char *name;     /* module name; also the name in a signed package */
    uint16_t tag;         /* must equal the tag in the module's image header */
    uint32_t offset;      /* in the module partition, multiple of ESPMOD_SLOT_ALIGN */
    uint32_t size;        /* multiple of ESPMOD_SLOT_ALIGN; xtensa: offset + size <= 1 MiB (ESP32) */
} espmod_slot_t;

typedef struct {
    const espmod_slot_t *slots;
    size_t slot_count;
    const char *partition;          /* data partition holding the slots, e.g. "data" */
    uint32_t default_stack_bytes;   /* for an image that asks for none */
} espmod_loader_config_t;

/* Loader errors (negative, distinct from ESPMOD_ERR_*). */
enum {
    ESPMOD_LOADER_ERR_SLOT = -101,    /* no such slot */
    ESPMOD_LOADER_ERR_IMAGE = -102,   /* no valid image in the slot */
    ESPMOD_LOADER_ERR_RUNNING = -103,
    ESPMOD_LOADER_ERR_NOT_RUNNING = -104,
    ESPMOD_LOADER_ERR_NOMEM = -105,
    ESPMOD_LOADER_ERR_MAP = -106,
    ESPMOD_LOADER_ERR_TIMEOUT = -107, /* the module did not return after a stop request */
    ESPMOD_LOADER_ERR_FLASH = -108,   /* erase or write failed */
};

typedef struct {
    bool installed;          /* a valid image header is present */
    bool running;
    uint32_t version;        /* of the installed package (0 if unknown) */
    int last_result;         /* what espmod_entry returned the last time; INT32_MIN before the first run */
    uint32_t runs;
    uint32_t last_runtime_ms;
    uint32_t stack_high_water_bytes;
    int last_check;          /* espmod_dmod_result_t of the last start attempt */
    bool needs_reboot;       /* the module stopped with memory it had not freed: the slot cannot be replaced until a restart */
} espmod_slot_status_t;

/* Once, after espmod_host_init(), NVS and the event loop (an upload needs the network). */
esp_err_t espmod_loader_init(const espmod_loader_config_t *config);

/* Start every installed module whose manifest asks for it (manifest key 6: its start request; `mkmod.py --autostart`). What
 * runs at boot is therefore decided by what is installed, not by the build. Returns how many were started. */
int espmod_loader_autostart(void);

/* The manifest of the module in `slot`, read from flash (the module need not run): its length, 0 if it has none, or
 * ESPMOD_ERR_SIZE / ESPMOD_LOADER_ERR_*. */
int espmod_loader_manifest(int slot, uint8_t *out, size_t capacity);

/* The module calls of the ABI (modules_info, module_start, module_stop), for espmod_host_hooks_t. The stop only queues. */
int espmod_loader_modules_info(uint8_t *out, size_t capacity);
int espmod_loader_start_named(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
int espmod_loader_stop_named(const uint8_t *name, size_t name_len);
int espmod_loader_manifest_named(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);

int espmod_loader_find(const char *name);     /* slot index or -1 */
const espmod_slot_t *espmod_loader_slot(int slot);
size_t espmod_loader_slot_count(void);

/* Map the module in `slot`, create its instance and queue HOST_START with `request` (copied, also kept for a
 * restart after an upgrade). Returns at once; the module's handlers run on the host's worker task. */
int espmod_loader_start(int slot, const uint8_t *request, size_t request_len);
/* Queue HOST_STOP and wait until the instance has ended and the slot is no longer mapped. 0 when it has (or was
 * not running). */
int espmod_loader_stop(int slot, uint32_t timeout_ms);   /* timeout 0: only queue the stop */
/* Hand a request for (component, method) to the running module (its inbox holds one), or do that and wait for its
 * respond(). */
int espmod_loader_post(int slot, int component, int method, const uint8_t *data, size_t len);
int espmod_loader_request(int slot, int component, int method, const uint8_t *data, size_t len,
                          uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms);
void espmod_loader_status(int slot, espmod_slot_status_t *out);

#ifdef __cplusplus
}
#endif
