/* SPDX-License-Identifier: Apache-2.0 */
/* The extent scan shared by Stage2 and the loader (components/xip_loader/include/xip_extent.h) over a fake flash. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xip_extent.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

#define PAGES 24
#define PAGE XIP_IMAGE_PAGE
static uint8_t flash[PAGES * XIP_IMAGE_PAGE];
static unsigned reads;

static int rd(void *ctx, uint32_t addr, void *buf, size_t len)
{
    (void)ctx;
    reads++;
    if (addr + len > sizeof(flash)) return -1;
    memcpy(buf, flash + addr, len);
    return 0;
}

static void extent_at(unsigned page, uint8_t kind, uint32_t length)
{
    uint8_t *p = flash + page * PAGE;
    p[0] = kind; p[1] = XIP_ABI_VERSION; p[2] = XIP_DMOD_HEADER_SIZE; p[3] = 0;
    memcpy(p + 4, &length, 4);
}

/* An ESP image with one segment of `body` bytes starting at the app description. */
static void runtime_at(unsigned page, uint32_t body, const char *version)
{
    uint8_t *p = flash + page * PAGE;
    p[0] = XIP_IMAGE_MAGIC; p[1] = 1; p[23] = 1;
    memcpy(p + 28, &body, 4);
    const uint32_t desc = XIP_IMAGE_DESC_MAGIC;
    memcpy(p + 32, &desc, 4);
    strcpy((char *)p + 32 + XIP_IMAGE_DESC_VERSION, version);
}

/* what Stage2 and mod_flash do to delete: the first byte of every page of the extent */
static void delete_extent(unsigned page, unsigned pages)
{
    for (unsigned i = 0; i < pages; i++) flash[(page + i) * PAGE] = XIP_EXTENT_DELETED;
}

int main(void)
{
    memset(flash, 0xff, sizeof(flash));
    xip_extent_t e[8];

    CHECK(xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_ALL, e, 8) == 0);        /* blank flash */
    CHECK(reads == PAGES);                                                              /* one 8-byte read per page */

    runtime_at(0, 0x40000, "5");                                                        /* 4 pages + header, extent 5 pages */
    extent_at(6, XIP_DMOD_KIND_MODULE, 0x10004);                                        /* two pages */
    extent_at(9, XIP_DMOD_KIND_MODULE, 0x2000);
    extent_at(11, XIP_EXTENT_DATA, 0x20000);                                            /* two pages of data */
    runtime_at(PAGES - 3, 0x20000, "7-3-gabc");                                         /* B near the end */
    memset(flash + 3 * PAGE, XIP_EXTENT_RUNTIME, 64);                                   /* a body page that looks like an image start */

    size_t n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_ALL, e, 8);
    CHECK(n == 5);
    CHECK(e[0].kind == XIP_EXTENT_RUNTIME && e[0].offset == 0 && e[0].version == 5 && e[0].size == 5 * PAGE);
    CHECK(e[1].kind == XIP_DMOD_KIND_MODULE && e[1].offset == 6 * PAGE && e[1].size == 2 * PAGE);
    CHECK(e[2].kind == XIP_DMOD_KIND_MODULE && e[2].offset == 9 * PAGE && e[2].size == PAGE);
    CHECK(e[3].kind == XIP_EXTENT_DATA && e[3].offset == 11 * PAGE && e[3].size == 2 * PAGE);
    CHECK(e[4].kind == XIP_EXTENT_RUNTIME && e[4].offset == (PAGES - 3) * PAGE && e[4].version == 7);

    /* only runtimes stored, the others skipped by length: the module body page that starts like an image is never looked at */
    memset(flash + 7 * PAGE, XIP_EXTENT_RUNTIME, 64);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_RUNTIME, e, 8);
    CHECK(n == 2 && e[0].offset == 0 && e[1].version == 7);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_RUNTIME, e, 1);            /* room for one */
    CHECK(n == 1 && e[0].offset == 0);

    /* a deleted extent: every page starts with 0x00, none is an extent, a later pass erases them back to 0xFF */
    delete_extent(0, 5);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_RUNTIME, e, 8);
    CHECK(n == 1 && e[0].version == 7);
    delete_extent(6, 2);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_MODULE, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);
    memset(flash, 0xff, 5 * PAGE);
    CHECK(xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_ALL, e, 8) == 3);

    /* lengths that are not believed */
    extent_at(14, XIP_DMOD_KIND_MODULE, 0x30000);                                       /* longer than the room left */
    extent_at(15, XIP_DMOD_KIND_MODULE, 10);                                            /* shorter than a header */
    extent_at(16, XIP_DMOD_KIND_MODULE, 0x2002);                                        /* not a multiple of 4 */
    extent_at(17, XIP_EXTENT_DATA, 0x400004);                                           /* the lead of an old "DMOD" image: not a data length */
    extent_at(18, XIP_EXTENT_DATA, 0x2100);                                             /* not whole sectors */
    n = xip_extent_scan(rd, NULL, 14 * PAGE, 2 * PAGE, XIP_WANT_ALL, e, 8);               /* the first: 3 pages in 2 */
    CHECK(n == 0);
    n = xip_extent_scan(rd, NULL, 15 * PAGE, 4 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 0);

    /* the area is a sub-range: the scan of the xip partition starts at its own base */
    n = xip_extent_scan(rd, NULL, 6 * PAGE, 4 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);

    /* a runtime whose segments run off the area is not one */
    runtime_at(20, 0x500000, "9");
    n = xip_extent_scan(rd, NULL, 20 * PAGE, 2 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 0);

    puts(failures ? "FAILED" : "extent scan ok");
    return failures != 0;
}
