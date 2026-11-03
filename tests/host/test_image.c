/* SPDX-License-Identifier: Apache-2.0 */
/* The runtime image rules shared by Stage2 and the loader (components/xip_loader/include/xip_image.h). */
#include <stdio.h>
#include <string.h>
#include "xip_image.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static uint8_t flash[0x40000];

static int rd(void *ctx, uint32_t off, void *buf, size_t n)
{
    (void)ctx;
    if (off + n > sizeof(flash)) return -1;
    memcpy(buf, flash + off, n);
    return 0;
}

static void le32(uint8_t *p, uint32_t v) { for (int i = 0; i < 4; i++) p[i] = (uint8_t)(v >> (8 * i)); }

/* An image of one segment of `len` bytes whose app description carries `version`. */
static void make(const char *version, uint32_t len, int hash)
{
    memset(flash, 0xff, sizeof(flash));
    flash[0] = XIP_IMAGE_MAGIC;
    flash[1] = 1;
    flash[23] = (uint8_t)hash;
    le32(flash + XIP_IMAGE_HEADER_SIZE, 0x42000020);
    le32(flash + XIP_IMAGE_HEADER_SIZE + 4, len);
    memset(flash + XIP_IMAGE_DESC_OFFSET, 0, 256);
    le32(flash + XIP_IMAGE_DESC_OFFSET, XIP_IMAGE_DESC_MAGIC);
    strncpy((char *)flash + XIP_IMAGE_DESC_OFFSET + XIP_IMAGE_DESC_VERSION, version, 31);
}

static xip_image_info_t look(void) { return xip_image_inspect(rd, NULL, 0x30000); }

int main(void)
{
    CHECK(xip_image_version("17", 2) == 17);
    CHECK(xip_image_version("17-3-gabc", 9) == 17);
    CHECK(xip_image_version("v17", 3) == 0);
    CHECK(xip_image_version("", 0) == 0);
    CHECK(xip_image_version("99999999999", 11) == UINT32_MAX);
    CHECK(xip_image_version("12", 1) == 1);              /* the length bounds the digits */

    make("42", 0x1000, 0);
    xip_image_info_t a = look();
    CHECK(a.valid && a.version == 42);
    CHECK(a.extent == 0x10000);                          /* 24 + 8 + 0x1000 + checksum fits one page */
    CHECK(a.length == 32 + 0x1000 + 16 - ((32 + 0x1000) & 15));   /* the exact bytes: the segment end, then the end of the checksum block */
    make("42", 0xffe0, 0);                               /* 32 + 0xffe0 = 0x10000: the checksum block spills into a second page */
    CHECK(look().extent == 0x20000);
    make("42", 0xffd0, 0);                               /* 32 + 0xffd0 = 0xfff0: padding and checksum end at 0x10000 */
    CHECK(look().extent == 0x10000);
    make("42", 0xffd0, 1);                               /* the appended SHA-256 is 32 bytes more */
    CHECK(look().extent == 0x20000);

    make("42", 0x1000, 0); flash[0] = 0xff;              /* an erased header: no image */
    CHECK(!look().valid);
    make("42", 0x1000, 0); flash[1] = 0;
    CHECK(!look().valid);
    make("42", 0x1000, 0); flash[1] = 17;
    CHECK(!look().valid);
    make("42", 0x1000, 0); le32(flash + XIP_IMAGE_DESC_OFFSET, 0);
    CHECK(!look().valid);                                /* no app description */
    make("42", 0x2ffff, 0);                              /* a segment longer than the partition */
    CHECK(!look().valid);
    make("42", 0x1000, 0);
    CHECK(!xip_image_inspect(rd, NULL, 100).valid);      /* a partition too small for a header */
    CHECK(!xip_image_inspect(rd, NULL, 0x1000).valid);   /* the image does not fit it */

    /* two slots */
    xip_image_info_t none = {0}, v5 = {.valid = true, .version = 5}, v7 = {.valid = true, .version = 7};
    xip_image_info_t both[2] = {v5, v7};
    CHECK(xip_image_pick(both, false) == 1);             /* the higher version */
    CHECK(xip_image_pick(both, true) == 0);              /* rollback: the other */
    CHECK(xip_image_older(both) == 0);
    both[0] = v7; both[1] = v5;
    CHECK(xip_image_pick(both, false) == 0 && xip_image_pick(both, true) == 1 && xip_image_older(both) == 1);
    both[1] = v7;
    CHECK(xip_image_pick(both, false) == 0);             /* a tie goes to the lower slot */
    CHECK(xip_image_pick(both, true) == 1);
    CHECK(xip_image_older(both) == -1);                  /* the same version twice: nothing to settle */
    xip_image_info_t one[2] = {none, v5};
    CHECK(xip_image_pick(one, false) == 1 && xip_image_pick(one, true) == 1 && xip_image_older(one) == -1);
    xip_image_info_t zero[2] = {none, none};
    CHECK(xip_image_pick(zero, false) == -1 && xip_image_pick(zero, true) == -1);

    puts(failures ? "FAILED" : "image rules ok");
    return failures != 0;
}
