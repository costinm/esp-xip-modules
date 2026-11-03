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

/* what mod_flash does to delete an M, D or X: byte 0 of the first page only; bytes 4-7 still give the length */
static void delete_extent(unsigned page)
{
    flash[page * PAGE] = XIP_EXTENT_DELETED;
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

    /* a deleted module or data extent: byte 0 programmed, the length still skips it (its body pages are not looked at) */
    delete_extent(6);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_MODULE, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_DEAD, e, 8);
    CHECK(n == 1 && e[0].kind == XIP_EXTENT_DELETED && e[0].offset == 6 * PAGE && e[0].size == 2 * PAGE);
    memset(flash + 7 * PAGE, XIP_EXTENT_RUNTIME, 64);                                   /* the second page of the deleted module: skipped */
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_RUNTIME, e, 8);
    CHECK(n == 2 && e[0].offset == 0 && e[1].version == 7);
    memset(flash, 0xff, 7 * PAGE);                                                      /* the runtime and the deleted module's header are gone */
    memset(flash + 7 * PAGE, 0xff, 64);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_ALL, e, 8);
    CHECK(n == 3);                                                                      /* module at 9, data at 11, runtime B */

    /* X is a data-kind extent; a module being written ('O') and a deleted one are skipped by their length, never taken for a module */
    extent_at(1, XIP_EXTENT_INDEX, 0x10000);
    extent_at(3, XIP_EXTENT_MODULE_PENDING, 0x10004);
    n = xip_extent_scan(rd, NULL, 0, 4 * PAGE + PAGE * 6, XIP_WANT_DATA, e, 8);
    CHECK(n == 1 && e[0].kind == XIP_EXTENT_INDEX && e[0].offset == PAGE);
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_MODULE, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);                                           /* the pending one at page 3 is not a module */
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_DEAD, e, 8);
    CHECK(n == 1 && e[0].kind == XIP_EXTENT_MODULE_PENDING && e[0].size == 2 * PAGE);
    memset(flash + 4 * PAGE, 0xff, 64);                                                 /* its second page (page 4) was never looked at */
    memset(flash + 4 * PAGE, XIP_DMOD_KIND_MODULE, 64);                                 /* ... even when it starts like a module */
    n = xip_extent_scan(rd, NULL, 0, sizeof(flash), XIP_WANT_MODULE, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);
    memset(flash, 0xff, 5 * PAGE);

    /* lengths are real byte counts: a module need not be a multiple of 4, nor data a multiple of a sector */
    extent_at(12, XIP_DMOD_KIND_MODULE, 0x2002);
    extent_at(13, XIP_EXTENT_DATA, 0x2100);
    extent_at(14, XIP_EXTENT_DATA, 0x10000);
    n = xip_extent_scan(rd, NULL, 12 * PAGE, 3 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 3 && e[0].size == PAGE && e[1].size == PAGE && e[2].size == PAGE);

    /* lengths that are not believed: the page is an orphan, one page */
    extent_at(15, XIP_DMOD_KIND_MODULE, 0x30000);                                       /* longer than the room left */
    extent_at(16, XIP_DMOD_KIND_MODULE, 64);                                            /* not above a header */
    extent_at(17, XIP_EXTENT_DATA, 0);                                                  /* zero */
    extent_at(18, XIP_EXTENT_DELETED, 0);
    extent_at(19, XIP_EXTENT_INDEX, 0x400004);
    extent_at(20, 0x5a, 0x10000);                                                       /* a first byte that is no kind */
    n = xip_extent_scan(rd, NULL, 15 * PAGE, 2 * PAGE, XIP_WANT_ALL | XIP_WANT_DEAD, e, 8);   /* the first: 3 pages in 2 */
    CHECK(n == 0);
    n = xip_extent_scan(rd, NULL, 16 * PAGE, 6 * PAGE, XIP_WANT_ALL | XIP_WANT_DEAD, e, 8);
    CHECK(n == 0);
    extent_at(17, XIP_EXTENT_DATA, 0x10000);
    n = xip_extent_scan(rd, NULL, 16 * PAGE, 6 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 1 && e[0].offset == 17 * PAGE);                                          /* an orphan is one page: the scan goes on */
    memset(flash + 12 * PAGE, 0xff, 9 * PAGE);

    /* the area is a sub-range: the scan of the xip partition starts at its own base */
    n = xip_extent_scan(rd, NULL, 6 * PAGE, 4 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 1 && e[0].offset == 9 * PAGE);

    /* a runtime whose segments run off the area, or that was cut (the later segment header is blank), is an orphan page */
    runtime_at(20, 0x500000, "9");
    n = xip_extent_scan(rd, NULL, 20 * PAGE, 2 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 0);
    memset(flash + 20 * PAGE, 0xff, PAGE);
    runtime_at(19, 0x1000, "9");
    flash[19 * PAGE + 1] = 2;                                                           /* two segments: the second header sits in blank flash */
    n = xip_extent_scan(rd, NULL, 19 * PAGE, 2 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 0);
    flash[19 * PAGE + 1] = 1;                                                           /* one segment: whole */
    n = xip_extent_scan(rd, NULL, 19 * PAGE, 2 * PAGE, XIP_WANT_ALL, e, 8);
    CHECK(n == 1 && e[0].offset == 19 * PAGE && e[0].size == PAGE);

    puts(failures ? "FAILED" : "extent scan ok");
    return failures != 0;
}
