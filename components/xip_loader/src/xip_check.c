/* SPDX-License-Identifier: Apache-2.0 */
#include <stddef.h>
#include "xip_check.h"

xip_dmod_result_t xip_dmod_check(const xip_dmod_header *h, const xip_slot_rules_t *r)
{
    if (h->magic != XIP_DMOD_MAGIC) return EMD_BAD_MAGIC;
    if (h->abi_version == 0 || h->abi_version > XIP_ABI_VERSION) return EMD_BAD_VERSION;
    if (h->header_size != XIP_DMOD_HEADER_SIZE || h->reserved0 != 0 || h->stack_bytes > r->max_stack_bytes ||
        h->state_bytes > 4096)
        return EMD_BAD_HEADER;
    for (size_t i = 0; i < sizeof(h->reserved); i++) if (h->reserved[i] != 0) return EMD_BAD_HEADER;
    if (h->image_size <= XIP_DMOD_HEADER_SIZE || h->image_size > r->slot_size) return EMD_BAD_SIZE;
    /* the manifest, if any, is inside the image after the header, 4-aligned */
    if (h->manifest_size || h->manifest_offset) {
        if (!h->manifest_size || h->manifest_offset < XIP_DMOD_HEADER_SIZE || (h->manifest_offset & 3) ||
            h->manifest_offset > h->image_size || h->manifest_size > h->image_size - h->manifest_offset) return EMD_BAD_HEADER;
    }
    /* RISC-V code may be 2-byte aligned; Xtensa entries are 4-byte aligned by the linker script. */
    if (h->entry_offset < XIP_DMOD_HEADER_SIZE || h->entry_offset >= h->image_size || (h->entry_offset & 1))
        return EMD_BAD_ENTRY;
    if (h->tag == 0) return EMD_BAD_HEADER;       /* the component number is the image's own; a module without one is not a module */
    if (r->fixed_vma) {
        /* Linked for one address: it must be the address this slot is mapped at. */
        if (h->flags != XIP_DMOD_FLAG_FIXED_VMA || h->code_vma != r->code_base + r->slot_offset ||
            h->data_vma != r->data_base + r->slot_offset) return EMD_BAD_PLACEMENT;
    } else if (h->flags != 0 || h->code_vma != 0 || h->data_vma != 0) {
        return EMD_BAD_PLACEMENT;
    }
    return EMD_OK;
}

const char *xip_dmod_reason(xip_dmod_result_t r)
{
    switch (r) {
    case EMD_OK: return "ok";
    case EMD_BAD_MAGIC: return "bad-magic";
    case EMD_BAD_VERSION: return "bad-abi-version";
    case EMD_BAD_HEADER: return "bad-header";
    case EMD_BAD_SIZE: return "bad-size";
    case EMD_BAD_ENTRY: return "bad-entry";
    case EMD_BAD_PLACEMENT: return "bad-placement";
    }
    return "unknown";
}
