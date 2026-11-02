/* SPDX-License-Identifier: Apache-2.0 */
/* The extent map of the xip partition: what sits on which 64 KiB pages. Pure C, no ESP-IDF: Stage2, the loader and the host tests compile this
 * one scan (modules/mod_flash has its own, independent implementation of the same format).
 *
 * The scan reads 8 bytes at the start of each page; byte 0 says what is there:
 *   0xFF            empty (erased)                         -> next page
 *   0x00            deleted (first byte programmed to 0)   -> next page; every page of a deleted extent starts with 0x00, none keeps old code
 *   'M' (module)    include/xip.h xip_dmod_header: kind, abi, header_size, image_size (bytes 4-7)  -> skip by image_size
 *   'D' (data)      the same lead, image_size = length; contents are mod_flash's (files, key-value)   -> skip by image_size
 *   0xE9            an ESP image (a runtime): its length is the end of its last segment (xip_image.h), its version the app description's
 *   anything else   not an extent                          -> next page
 * An extent spans whole pages. A page inside an extent is never looked at. */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "xip.h"
#include "xip_image.h"

#define XIP_WANT_RUNTIME 1u
#define XIP_WANT_MODULE 2u
#define XIP_WANT_DATA 4u
#define XIP_WANT_ALL 7u

typedef struct {
    uint32_t offset;    /* flash address, a page boundary */
    uint32_t size;      /* bytes, whole pages */
    uint8_t kind;       /* the first byte: XIP_EXTENT_RUNTIME, XIP_DMOD_KIND_MODULE or XIP_EXTENT_DATA */
    uint32_t version;   /* a runtime: the number of its version string; otherwise 0 (a module's version is in its manifest) */
} xip_extent_t;

/* Reads `len` bytes at the flash address `addr`. 0 on success. */
typedef xip_image_read_fn xip_extent_read_fn;

typedef struct { xip_extent_read_fn read; void *ctx; uint32_t base; } xip_extent_view_t;

static inline int xip_extent_shifted(void *ctx, uint32_t off, void *buf, size_t len)
{
    const xip_extent_view_t *v = ctx;
    return v->read(v->ctx, v->base + off, buf, len);
}

static inline unsigned xip_extent_want(uint8_t kind)
{
    return kind == XIP_EXTENT_RUNTIME ? XIP_WANT_RUNTIME : kind == XIP_DMOD_KIND_MODULE ? XIP_WANT_MODULE : kind == XIP_EXTENT_DATA ? XIP_WANT_DATA : 0;
}

/* Is there an extent at the page `at`, with `room` bytes up to the end of the area? */
static inline bool xip_extent_probe(xip_extent_read_fn read, void *ctx, uint32_t at, uint32_t room, xip_extent_t *out)
{
    uint8_t lead[8];
    if (room < XIP_IMAGE_PAGE || read(ctx, at, lead, sizeof(lead))) return false;
    const uint8_t kind = lead[0];
    if (kind == XIP_DMOD_KIND_MODULE || kind == XIP_EXTENT_DATA) {
        const uint32_t length = xip_image_le32(lead + 4);
        if (length <= sizeof(lead) || length > room) return false;
        if (kind == XIP_DMOD_KIND_MODULE ? (length <= XIP_DMOD_HEADER_SIZE || (length & 3u)) : (length & 0xfffu)) return false;
        const uint32_t size = (length + XIP_IMAGE_PAGE - 1) / XIP_IMAGE_PAGE * XIP_IMAGE_PAGE;
        if (size > room) return false;
        *out = (xip_extent_t){.offset = at, .size = size, .kind = kind};
        return true;
    }
    if (kind == XIP_EXTENT_RUNTIME) {
        xip_extent_view_t view = {read, ctx, at};
        const xip_image_info_t info = xip_image_inspect(xip_extent_shifted, &view, room < XIP_RUNTIME_RESERVE ? room : XIP_RUNTIME_RESERVE);
        if (!info.valid) return false;
        *out = (xip_extent_t){.offset = at, .size = info.extent, .kind = kind, .version = info.version};
        return true;
    }
    return false;
}

/* Every extent of the area [base, base + size) in address order. Only the kinds in `want` (XIP_WANT_*) are stored, at most `max`; the others
 * are skipped over by their length. Returns the number stored. */
static inline size_t xip_extent_scan(xip_extent_read_fn read, void *ctx, uint32_t base, uint32_t size, unsigned want, xip_extent_t *out, size_t max)
{
    size_t n = 0;
    for (uint32_t at = base; at + XIP_IMAGE_PAGE <= base + size;) {
        xip_extent_t e;
        if (!xip_extent_probe(read, ctx, at, base + size - at, &e)) { at += XIP_IMAGE_PAGE; continue; }
        if ((want & xip_extent_want(e.kind)) && n < max) out[n++] = e;
        at += e.size;
    }
    return n;
}
