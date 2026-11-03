/* SPDX-License-Identifier: Apache-2.0 */
/* What Stage2 and the loader both need to know about a runtime image, as pure functions over its bytes. Where the images are is the extent
 * scan's business (xip_extent.h). Header-only and free of ESP-IDF headers so the bootloader, the runtime and the host tests compile it.
 *
 * There is no "active slot" record. The image itself says: a slot holds an image when its first byte is the ESP image magic, and the image's
 * version is the leading number of the version string of its app description (build.sh sets it; no digits = 0). Stage2 boots the highest
 * version. Two valid images mean the newer one is on trial: if it crashes, Stage2 erases it and boots the older; if the runtime stays up, it
 * erases the older one itself (xip_loader_settle). One valid image is a settled runtime; the rest of both partitions is module space. */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define XIP_IMAGE_MAGIC 0xe9u            /* esp_image_header_t.magic */
#define XIP_IMAGE_HEADER_SIZE 24u        /* esp_image_header_t */
#define XIP_IMAGE_SEGMENT_HEADER 8u      /* esp_image_segment_header_t: load address, data length */
#define XIP_IMAGE_DESC_OFFSET (XIP_IMAGE_HEADER_SIZE + XIP_IMAGE_SEGMENT_HEADER)
#define XIP_IMAGE_DESC_MAGIC 0xabcd5432u /* esp_app_desc_t.magic_word */
#define XIP_IMAGE_DESC_VERSION 16u       /* offset of the 32-byte version string inside esp_app_desc_t */
#define XIP_IMAGE_HEAD_BYTES (XIP_IMAGE_DESC_OFFSET + XIP_IMAGE_DESC_VERSION + 32u)
#define XIP_RUNTIME_RESERVE 0x180000u    /* the most a runtime image may take: the size of one slot */
#define XIP_IMAGE_PAGE 0x10000u          /* the flash MMU page: module space is counted in these */
#define XIP_IMAGE_MAX_SEGMENTS 16u

typedef struct {
    bool valid;          /* magic, segment count and app description are plausible (the full checksum is the bootloader's) */
    uint32_t version;
    uint32_t length;     /* bytes of the image: the end of its last segment, the checksum block and the appended hash (docs/flash-format.md section 2) */
    uint32_t extent;     /* `length` rounded up to XIP_IMAGE_PAGE; the pages after it are module space */
} xip_image_info_t;

/* Reads `len` bytes at `off` of the partition. 0 on success. */
typedef int (*xip_image_read_fn)(void *ctx, uint32_t off, void *buf, size_t len);

static inline uint32_t xip_image_le32(const uint8_t *p)
{
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

/* The leading decimal digits of a version string ("17", "17-3-gabc" -> 17), saturating; 0 without digits. */
static inline uint32_t xip_image_version(const char *s, size_t n)
{
    uint64_t v = 0;
    for (size_t i = 0; i < n && s[i] >= '0' && s[i] <= '9'; i++) {
        v = v * 10 + (uint32_t)(s[i] - '0');
        if (v > UINT32_MAX) return UINT32_MAX;
    }
    return (uint32_t)v;
}

/* Looks at the image at the start of a partition of `part_size` bytes. */
static inline xip_image_info_t xip_image_inspect(xip_image_read_fn read, void *ctx, uint32_t part_size)
{
    xip_image_info_t none = {0};
    uint8_t head[XIP_IMAGE_HEAD_BYTES];
    if (part_size < sizeof(head) || read(ctx, 0, head, sizeof(head))) return none;
    if (head[0] != XIP_IMAGE_MAGIC || head[1] == 0 || head[1] > XIP_IMAGE_MAX_SEGMENTS) return none;
    if (xip_image_le32(head + XIP_IMAGE_DESC_OFFSET) != XIP_IMAGE_DESC_MAGIC) return none;
    const uint8_t segments = head[1], hash_appended = head[23];
    uint32_t pos = XIP_IMAGE_HEADER_SIZE;
    for (uint8_t i = 0; i < segments; i++) {
        uint8_t seg[XIP_IMAGE_SEGMENT_HEADER];
        if (pos > part_size - sizeof(seg) || read(ctx, pos, seg, sizeof(seg))) return none;
        const uint32_t len = xip_image_le32(seg + 4);
        if (len > part_size - pos - XIP_IMAGE_SEGMENT_HEADER) return none;
        pos += XIP_IMAGE_SEGMENT_HEADER + len;
    }
    pos = (pos | 15u) + 1u;                 /* the checksum byte ends a 16-byte block */
    if (hash_appended) pos += 32u;
    if (pos > part_size) return none;
    xip_image_info_t info = {.valid = true, .length = pos, .extent = (pos + XIP_IMAGE_PAGE - 1) / XIP_IMAGE_PAGE * XIP_IMAGE_PAGE,
                             .version = xip_image_version((const char *)head + XIP_IMAGE_DESC_OFFSET + XIP_IMAGE_DESC_VERSION, 32)};
    return info;
}

/* Which of the two slots to boot: the highest version, the lower slot on a tie. `previous`: the other valid one when there is one (a manual
 * rollback: the entry button), else the same as without it. -1: no valid image. */
static inline int xip_image_pick(const xip_image_info_t info[2], bool previous)
{
    int best = -1;
    for (int i = 0; i < 2; i++)
        if (info[i].valid && (best < 0 || info[i].version > info[best].version)) best = i;
    if (previous && best >= 0 && info[1 - best].valid) return 1 - best;
    return best;
}

/* Two valid images: the newer is on trial. Which slot would be settled by erasing, or -1 when the runtime is already settled. */
static inline int xip_image_older(const xip_image_info_t info[2])
{
    if (!info[0].valid || !info[1].valid || info[0].version == info[1].version) return -1;
    return info[0].version < info[1].version ? 0 : 1;
}
