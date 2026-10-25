/* SPDX-License-Identifier: Apache-2.0 */
/* The checks a device makes before it maps or installs a module. Pure C, no ESP-IDF: they run in
 * the host tests (tests/host) exactly as on the device.
 *
 *  - xip_dmod_check: the 64-byte image header (include/xip.h) against the place it was found at
 * The package around an image on the upload port is modules/mod_flash's business. */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "xip.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Xtensa: the link address belongs to the module, not to its flash page. A module starts at a slot of one 64 KiB MMU page and may span consecutive slots, which is the
 * index of its RAM cell ((static_addr - static_base) / XIP_DMOD_STATIC_SLOT_BYTES); the image is linked at code_base + slot * this
 * (data_base likewise) and may sit on any flash page, because the loader maps whatever page it was found on at that address. */
#define XIP_DMOD_WINDOW_SLOT_BYTES 0x10000u

/* ---- module image header ---- */
typedef enum {
    EMD_OK = 0,
    EMD_BAD_MAGIC = -1,
    EMD_BAD_VERSION = -2,    /* newer than this host, or zero */
    EMD_BAD_HEADER = -3,     /* header_size, reserved bytes, stack */
    EMD_BAD_SIZE = -4,       /* image_size outside the room it was found in */
    EMD_BAD_ENTRY = -5,
    EMD_BAD_PLACEMENT = -8,  /* flags / VMAs do not match what this chip and slot require */
} xip_dmod_result_t;

typedef struct {
    uint32_t slot_offset;    /* where the image was found in the module partition (informational: no placement rule depends on it) */
    uint32_t slot_size;      /* the room from there to the next image or the end of the partition */
    uint32_t max_stack_bytes;
    bool fixed_vma;          /* this chip runs modules at a fixed address (Xtensa) */
    uint32_t static_base;    /* platform's reserved module-context RAM arena */
    uint32_t code_base, data_base; /* fixed_vma: window origins; image VMA = base + window slot * XIP_DMOD_WINDOW_SLOT_BYTES */
    uint32_t window_bytes;   /* fixed_vma: size of the window; 0: XIP_DMOD_STATIC_SLOTS slots */
} xip_slot_rules_t;

xip_dmod_result_t xip_dmod_check(const xip_dmod_header *h, const xip_slot_rules_t *rules);
const char *xip_dmod_reason(xip_dmod_result_t r);

#ifdef __cplusplus
}
#endif
