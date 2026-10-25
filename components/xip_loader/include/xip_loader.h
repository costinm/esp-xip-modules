/* SPDX-License-Identifier: Apache-2.0 */
/* Runs module images from flash. A module is not a task: its image stays mapped
 * while the host's worker calls its handler.
 *
 * The loader scans the module partition: every flash page (XIP_SLOT_ALIGN) that starts with an image header is an image of a module, named by its
 * manifest (key 1), versioned by it (key 8, a plain number, absent = 0) and numbered by its header (the component). Per name the highest version
 * is the module (the slot list); older ones are shadowed (xip_loader_shadow). Nobody tells it where a module lives or what it is called. It maps an
 * image, runs it on the host and stops it; it has no network, crypto, or image installer. It knows no module by name.
 * Mapping, tasks and the host table are the part that depends on the chip; the checks before a
 * module is mapped are in xip_check.h. */
#pragma once

#include <stdbool.h>
#include "esp_err.h"
#include "xip.h"
#include "xip_select.h"

#ifdef __cplusplus
extern "C" {
#endif

#define XIP_SLOT_ALIGN 0x10000u   /* the flash MMU page: images start and end on it */

/* What a scan found: the loader's index of a module is its position in the order found (by offset), valid until the next rescan. */
typedef struct {
    const char *name;     /* from the manifest of the image (key 1), or "@<offset>" without one */
    uint16_t tag;         /* the component number: the tag in the image header */
    uint32_t offset;      /* in the module partition, multiple of XIP_SLOT_ALIGN */
    uint32_t size;        /* the physical flash pages the image spans */
    uint32_t version;     /* manifest key 8: higher is newer; 0 without one */
} xip_slot_t;

/* An image the scan did not pick: a lower version of an active module (shadowed: kept for recovery, its page is the first to be reused),
 * or the same version twice (duplicate: the lowest page is the active one). */
typedef struct {
    const char *name;
    uint16_t tag;
    uint32_t offset;       /* in the module partition */
    uint32_t version;
    uint32_t shadowed_by;  /* offset of the active image of this name */
    bool duplicate;
} xip_shadow_t;

typedef struct {
    const char *partition;          /* data partition holding the modules, e.g. "data" */
    uint32_t default_stack_bytes;   /* for an image that asks for none */
} xip_loader_config_t;

/* Loader errors (negative, distinct from XIP_ERR_*). */
enum {
    XIP_LOADER_ERR_SLOT = -101,    /* no such module index */
    XIP_LOADER_ERR_IMAGE = -102,   /* the image is not acceptable (see last_check) */
    XIP_LOADER_ERR_RUNNING = -103,
    XIP_LOADER_ERR_NOT_RUNNING = -104,
    XIP_LOADER_ERR_NOMEM = -105,
    XIP_LOADER_ERR_MAP = -106,
    XIP_LOADER_ERR_TIMEOUT = -107, /* the module did not return after a stop request */
    XIP_LOADER_ERR_FLASH = -108,   /* erase or write failed */
};

typedef struct {
    bool installed;          /* a valid image header is present */
    bool running;
    uint32_t version;        /* of the installed image: manifest key 8 (0 if it has none) */
    int last_result;         /* what xip_entry returned the last time; INT32_MIN before the first run */
    uint32_t runs;
    uint32_t last_runtime_ms;
    uint32_t stack_high_water_bytes;
    int last_check;          /* xip_dmod_result_t of the last start attempt */
    bool needs_reboot;       /* the module stopped with memory it had not freed: the slot cannot be replaced until a restart */
} xip_slot_status_t;

/* Once, after xip_host_init(), NVS and the event loop (an upload needs the network). Scans the partition. */
esp_err_t xip_loader_init(const xip_loader_config_t *config);

/* Start every installed module whose manifest asks for it (manifest key 6: its start request; `mkmod.py --autostart`). What
 * runs at boot is therefore decided by what is installed, not by the build. Returns how many were started. */
int xip_loader_autostart(void);

/* The manifest of the module in `slot`, read from flash (the module need not run): its length, 0 if it has none, or
 * XIP_ERR_SIZE / XIP_LOADER_ERR_*. */
int xip_loader_manifest(int slot, uint8_t *out, size_t capacity);

/* The module calls of the ABI (modules_info, module_start, module_stop), for xip_host_hooks_t. The stop only queues. */
int xip_loader_modules_info(uint8_t *out, size_t capacity);
int xip_loader_start_named(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
int xip_loader_stop_named(const uint8_t *name, size_t name_len);
/* Start the installed module that hosts the component `component` (manifest key 11), unless it runs: the `component_start` hook of the runtime's
 * get_component. 0: running now; XIP_ERR_NOTFOUND: no installed module names it; otherwise the start's error. */
int xip_loader_start_hosting(const char *component);
int xip_loader_manifest_named(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);

/* Look at the partition again (after an install): the highest version of each name becomes the module, older ones shadowed. A running
 * module keeps running; its page must not have been rewritten. Slot indices may change. Returns the number of modules, or negative. */
int xip_loader_rescan(void);
size_t xip_loader_shadow_count(void);
const xip_shadow_t *xip_loader_shadow(int index);
/* Start the image of `name` with exactly `version`, active or shadowed. A shadowed version replaces the active one in the slot list until the
 * next rescan, and only when the active one is not running. XIP_ERR_NOTFOUND when no such version is installed. */
int xip_loader_start_version(const uint8_t *name, size_t name_len, uint32_t version, const uint8_t *request, size_t request_len);

int xip_loader_find(const char *name);     /* module index or -1 */
const xip_slot_t *xip_loader_slot(int slot);
size_t xip_loader_slot_count(void);

/* Map the module in `slot`, create its instance and queue HOST_START with `request` (copied, also kept for a
 * restart after an upgrade). Returns at once; the module's handlers run on the host's worker task. */
int xip_loader_start(int slot, const uint8_t *request, size_t request_len);
/* Queue HOST_STOP and wait until the instance has ended and the slot is no longer mapped. 0 when it has (or was
 * not running). */
int xip_loader_stop(int slot, uint32_t timeout_ms);   /* timeout 0: only queue the stop */
/* Hand a request for (component, method) to the running module (its inbox holds one), or do that and wait for its
 * reply (a message to the application). */
int xip_loader_post(int slot, int component, int method, const uint8_t *data, size_t len);
int xip_loader_request(int slot, int component, int method, const uint8_t *data, size_t len,
                          uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms);
void xip_loader_status(int slot, xip_slot_status_t *out);

#ifdef __cplusplus
}
#endif
