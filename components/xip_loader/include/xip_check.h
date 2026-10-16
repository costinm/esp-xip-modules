/* SPDX-License-Identifier: Apache-2.0 */
/* The checks a device makes before it maps or installs a module. Pure C, no ESP-IDF: they run in
 * the host tests (tests/host) exactly as on the device.
 *
 *  - xip_dmod_check: the 64-byte image header (include/xip.h) against the slot it sits in
 * The signed package around an image is esp-wifi-loader's business (docs/protocol.md). */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "xip.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ---- module image header ---- */
typedef enum {
    EMD_OK = 0,
    EMD_BAD_MAGIC = -1,
    EMD_BAD_VERSION = -2,    /* newer than this host, or zero */
    EMD_BAD_HEADER = -3,     /* header_size, reserved bytes, stack */
    EMD_BAD_SIZE = -4,       /* image_size outside the slot */
    EMD_BAD_ENTRY = -5,
    EMD_WRONG_TAG = -6,
    EMD_MISSING_FEATURE = -7,
    EMD_BAD_PLACEMENT = -8,  /* flags / VMAs do not match what this chip and slot require */
} xip_dmod_result_t;

typedef struct {
    uint16_t tag;            /* tag of the slot */
    uint32_t slot_offset;    /* offset of the slot in the module partition */
    uint32_t slot_size;
    uint32_t host_features;  /* XIP_FEAT_* the host provides */
    uint32_t max_stack_bytes;
    bool fixed_vma;          /* this chip runs modules at a fixed address (Xtensa) */
    uint32_t code_base, data_base; /* fixed_vma: window origins; image VMA = base + slot_offset */
} xip_slot_rules_t;

xip_dmod_result_t xip_dmod_check(const xip_dmod_header *h, const xip_slot_rules_t *rules);
const char *xip_dmod_reason(xip_dmod_result_t r);

#ifdef __cplusplus
}
#endif
