/* SPDX-License-Identifier: Apache-2.0 */
/* Which image of a name runs: the pure selection rule of the loader scan (no ESP-IDF; tests/host/test_select.c runs it as on the device).
 *
 * The scan lists every valid image page by page, ascending. Per name the image with the highest version is ACTIVE; lower versions are
 * SHADOWED (kept for recovery and flash leveling); an image of the same name and version as the winner is a DUPLICATE (the lowest page
 * wins). Two active images with overlapping module slots cannot both run: the later page is a CELL_CONFLICT and is ignored.
 * The cell rule concerns active images only: versions of one module share a cell by design. */
#pragma once

#include <stddef.h>
#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XIP_SELECT_NAME_BYTES 24

typedef struct {
    uint32_t page;            /* flash page (offset / 64 KiB) in the module partition; the candidates are in ascending order */
    uint32_t version;         /* manifest key 8, 0 without one */
    uint32_t static_addr;     /* the module RAM cell the image declares */
    char name[XIP_SELECT_NAME_BYTES];
    uint32_t window_slots;    /* Xtensa mapped span, 0/1 means one; PIC holds only its primary RAM cell. */
} xip_candidate_t;

typedef enum {
    XIP_ROLE_ACTIVE = 0,
    XIP_ROLE_SHADOWED,        /* same name, lower version than `by` */
    XIP_ROLE_DUPLICATE,       /* same name and version as `by`, higher page */
    XIP_ROLE_CELL_CONFLICT,   /* another active module (`by`) overlaps the slot range */
} xip_role_t;

typedef struct {
    xip_role_t role;
    int by;                   /* index of the candidate that shadows / duplicates / holds the cell; -1 for ACTIVE */
} xip_choice_t;

/* Shared scan/activation overlap check. Addresses identify primary RAM cells;
 * adjacent cells correspond to adjacent fixed code/data windows. */
bool xip_slots_overlap(uint32_t a, uint32_t a_slots, uint32_t b, uint32_t b_slots);

/* `out` has room for n entries. */
void xip_select(const xip_candidate_t *c, size_t n, xip_choice_t *out);

#ifdef __cplusplus
}
#endif
