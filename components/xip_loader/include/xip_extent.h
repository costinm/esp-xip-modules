/* SPDX-License-Identifier: Apache-2.0 */
/* The extent map of the xip partition: what sits on which 64 KiB pages (docs/flash-format.md section 2). Pure C, no ESP-IDF: Stage2, the loader and
 * the host tests compile this one scan (modules/mod_flash has its own, independent implementation of the same format).
 *
 * The scan reads 8 bytes at the start of each page; byte 0 says what is there:
 *   0xFF            empty page                             -> one page
 *   0xE9            an ESP image (a runtime): its length is found by walking its segments (xip_image.h), its version is the app description's
 *   'M'             a module: include/xip.h xip_dmod_header, image_size (bytes 4-7) above the 64-byte header
 *   'O'             a module being written ('M' with bit 1 set): skipped like 'M', never run
 *   'D', 'X'        data and index extents, mod_flash's: bytes 4-7 are the length (one page)
 *   0x00            a deleted M, D or X: only byte 0 was programmed, so bytes 4-7 still give the length
 *   anything else   an orphan page: one page
 * The length of every kind but 0xE9 is the real byte count at bytes 4-7, not a multiple of anything; a length of zero or one that passes the end of
 * the area makes the page an orphan. An extent spans ceil(length / 64 KiB) pages; a page inside an extent is never looked at. An 0xE9 page whose
 * walk fails is an orphan too. Stage2 wants only runtimes and the loader runtimes and modules; the others are skipped by their length. */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "xip.h"
#include "xip_image.h"

/* The first byte of an extent (the module kind, XIP_DMOD_KIND_MODULE, is in xip.h). These describe the layout of the partition, not the ABI. */
#define XIP_EXTENT_DATA 'D'          /* data: bytes 4-7 are its length in bytes */
#define XIP_EXTENT_INDEX 'X'         /* index: as data */
#define XIP_EXTENT_MODULE_PENDING 'O' /* a module being written: 'M' with bit 1 set; the writer clears it last */
#define XIP_EXTENT_RUNTIME 0xe9u     /* an ESP image */
#define XIP_EXTENT_EMPTY 0xffu       /* a page never written, or erased */
#define XIP_EXTENT_DELETED 0x00u     /* a deleted M, D or X: only byte 0 was programmed, bytes 4-7 still give the length */

#define XIP_WANT_RUNTIME 1u
#define XIP_WANT_MODULE 2u
#define XIP_WANT_DATA 4u     /* D and X */
#define XIP_WANT_ALL 7u
#define XIP_WANT_DEAD 8u     /* deleted and pending extents: not in ALL */
#define XIP_WANT_STOP 16u    /* Stage2 only: honour XIP_BOOT_STOP after a runtime at the start of the area */
/* The byte after the runtime at the start of the area: 0xF0..0xFE stops the scan (0xFE: fresh boot, the rest of the area is unformatted;
 * 0xFC: recovery boot, the rest holds extents but the front region may hold stale fragments; 0xF8: formatting done, a single image). 0xFF (blank) and anything else: keep scanning.
 * Only the runtime and its users interpret it beyond that; mod_flash clears bit 7 (0x78) when it installs a second image. */
#define XIP_BOOT_STOP 0xf0u
#define XIP_EXTENT_MAX_PAGES 255u

typedef struct {
    uint32_t offset;    /* flash address, a page boundary */
    uint32_t size;      /* bytes, whole pages */
    uint8_t kind;       /* the first byte: XIP_EXTENT_RUNTIME, XIP_DMOD_KIND_MODULE, XIP_EXTENT_MODULE_PENDING, XIP_EXTENT_DATA, XIP_EXTENT_INDEX or XIP_EXTENT_DELETED */
    uint8_t after;      /* a runtime: the byte right after the image (0 when out of the area); see XIP_BOOT_STOP */
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
    switch (kind) {
    case XIP_EXTENT_RUNTIME: return XIP_WANT_RUNTIME;
    case XIP_DMOD_KIND_MODULE: return XIP_WANT_MODULE;
    case XIP_EXTENT_DATA: case XIP_EXTENT_INDEX: return XIP_WANT_DATA;
    case XIP_EXTENT_MODULE_PENDING: case XIP_EXTENT_DELETED: return XIP_WANT_DEAD;
    default: return 0;
    }
}

/* Is there an extent at the page `at`, with `room` bytes up to the end of the area? Empty and orphan pages are not extents. */
static inline bool xip_extent_probe(xip_extent_read_fn read, void *ctx, uint32_t at, uint32_t room, xip_extent_t *out)
{
    uint8_t lead[8];
    if (room < XIP_IMAGE_PAGE || read(ctx, at, lead, sizeof(lead))) return false;
    const uint8_t kind = lead[0];
    if (kind == XIP_EXTENT_RUNTIME) {
        xip_extent_view_t view = {read, ctx, at};
        const uint32_t cap = XIP_EXTENT_MAX_PAGES * XIP_IMAGE_PAGE;
        const xip_image_info_t info = xip_image_inspect(xip_extent_shifted, &view, room < cap ? room : cap);
        if (!info.valid) return false;
        uint8_t after = 0;
        if (info.length < room && read(ctx, at + info.length, &after, 1)) after = 0;
        *out = (xip_extent_t){.offset = at, .size = info.extent, .kind = kind, .after = after, .version = info.version};
        return true;
    }
    if (!xip_extent_want(kind)) return false;
    const uint32_t length = xip_image_le32(lead + 4);
    if (length == 0 || length > room) return false;
    if ((kind == XIP_DMOD_KIND_MODULE || kind == XIP_EXTENT_MODULE_PENDING) && length <= XIP_DMOD_HEADER_SIZE) return false;
    const uint32_t size = (length + XIP_IMAGE_PAGE - 1) / XIP_IMAGE_PAGE * XIP_IMAGE_PAGE;
    if (size > room) return false;
    *out = (xip_extent_t){.offset = at, .size = size, .kind = kind};
    return true;
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
        if ((want & XIP_WANT_STOP) && at == base && e.kind == XIP_EXTENT_RUNTIME && e.after >= XIP_BOOT_STOP && e.after != 0xffu) break;   /* a flasher's image: the rest is not parsed */
        at += e.size;
    }
    return n;
}
