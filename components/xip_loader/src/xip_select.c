/* SPDX-License-Identifier: Apache-2.0 */
#include <string.h>
#include "xip_select.h"
#include "xip.h"

bool xip_slots_overlap(uint32_t a, uint32_t a_slots, uint32_t b, uint32_t b_slots)
{
    const uint64_t ae = (uint64_t)a + (a_slots ? a_slots : 1) * (uint64_t)XIP_DMOD_STATIC_SLOT_BYTES;
    const uint64_t be = (uint64_t)b + (b_slots ? b_slots : 1) * (uint64_t)XIP_DMOD_STATIC_SLOT_BYTES;
    return (uint64_t)a < be && (uint64_t)b < ae;
}


void xip_select(const xip_candidate_t *c, size_t n, xip_choice_t *out)
{
    /* per name: the highest version wins, the lowest page among equal versions (candidates come in ascending pages) */
    for (size_t i = 0; i < n; i++) {
        size_t w = i;
        for (size_t j = 0; j < n; j++) {
            if (strncmp(c[j].name, c[i].name, XIP_SELECT_NAME_BYTES)) continue;
            if (c[j].version > c[w].version || (c[j].version == c[w].version && c[j].page < c[w].page)) w = j;
        }
        if (w == i) out[i] = (xip_choice_t){XIP_ROLE_ACTIVE, -1};
        else out[i] = (xip_choice_t){c[i].version == c[w].version ? XIP_ROLE_DUPLICATE : XIP_ROLE_SHADOWED, (int)w};
    }
    /* Active module slot ranges must not overlap: the lowest page keeps its range. */
    for (size_t i = 0; i < n; i++) {
        if (out[i].role != XIP_ROLE_ACTIVE) continue;
        for (size_t j = 0; j < i; j++) {
            if (out[j].role == XIP_ROLE_ACTIVE && xip_slots_overlap(c[j].static_addr, c[j].window_slots, c[i].static_addr, c[i].window_slots)) {
                out[i] = (xip_choice_t){XIP_ROLE_CELL_CONFLICT, (int)j};
                break;
            }
        }
    }
}
