/* SPDX-License-Identifier: Apache-2.0 */
#include <stddef.h>
#include "espmod_check.h"

espmod_dmod_result_t espmod_dmod_check(const espmod_dmod_header *h, const espmod_slot_rules_t *r)
{
    if (h->magic != ESPMOD_DMOD_MAGIC) return EMD_BAD_MAGIC;
    if (h->abi_version == 0 || h->abi_version > ESPMOD_ABI_VERSION) return EMD_BAD_VERSION;
    if (h->header_size != ESPMOD_DMOD_HEADER_SIZE || h->reserved0 != 0 || h->stack_bytes > r->max_stack_bytes ||
        h->state_bytes > 4096)
        return EMD_BAD_HEADER;
    for (size_t i = 0; i < sizeof(h->reserved); i++) if (h->reserved[i] != 0) return EMD_BAD_HEADER;
    if (h->image_size <= ESPMOD_DMOD_HEADER_SIZE || h->image_size > r->slot_size) return EMD_BAD_SIZE;
    /* the manifest, if any, is inside the image after the header, 4-aligned */
    if (h->manifest_size || h->manifest_offset) {
        if (!h->manifest_size || h->manifest_offset < ESPMOD_DMOD_HEADER_SIZE || (h->manifest_offset & 3) ||
            h->manifest_offset > h->image_size || h->manifest_size > h->image_size - h->manifest_offset) return EMD_BAD_HEADER;
    }
    /* RISC-V code may be 2-byte aligned; Xtensa entries are 4-byte aligned by the linker script. */
    if (h->entry_offset < ESPMOD_DMOD_HEADER_SIZE || h->entry_offset >= h->image_size || (h->entry_offset & 1))
        return EMD_BAD_ENTRY;
    if (h->tag != r->tag) return EMD_WRONG_TAG;
    if ((h->required_features & ~r->host_features) != 0) return EMD_MISSING_FEATURE;
    if (r->fixed_vma) {
        /* Linked for one address: it must be the address this slot is mapped at. */
        if (h->flags != ESPMOD_DMOD_FLAG_FIXED_VMA || h->code_vma != r->code_base + r->slot_offset ||
            h->data_vma != r->data_base + r->slot_offset) return EMD_BAD_PLACEMENT;
    } else if (h->flags != 0 || h->code_vma != 0 || h->data_vma != 0) {
        return EMD_BAD_PLACEMENT;
    }
    return EMD_OK;
}

const char *espmod_dmod_reason(espmod_dmod_result_t r)
{
    switch (r) {
    case EMD_OK: return "ok";
    case EMD_BAD_MAGIC: return "bad-magic";
    case EMD_BAD_VERSION: return "bad-abi-version";
    case EMD_BAD_HEADER: return "bad-header";
    case EMD_BAD_SIZE: return "bad-size";
    case EMD_BAD_ENTRY: return "bad-entry";
    case EMD_WRONG_TAG: return "wrong-tag";
    case EMD_MISSING_FEATURE: return "missing-host-feature";
    case EMD_BAD_PLACEMENT: return "bad-placement";
    }
    return "unknown";
}
