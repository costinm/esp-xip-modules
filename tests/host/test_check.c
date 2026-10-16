/* SPDX-License-Identifier: Apache-2.0 */
/* The checks a device makes before mapping or installing a module (components/xip_loader/src/xip_check.c).
 * With a package file argument: the image inside a signed package (made by sign.py) passes the slot rules. The
 * package itself is esp-wifi-loader's to verify. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xip_check.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static xip_dmod_header good(void)
{
    xip_dmod_header h = {.magic = XIP_DMOD_MAGIC, .abi_version = XIP_ABI_VERSION, .header_size = 64,
                            .entry_offset = 0x44, .image_size = 0x200, .tag = 2};
    return h;
}

static void test_dmod(void)
{
    xip_slot_rules_t pic = {.tag = 2, .slot_offset = 0x10000, .slot_size = 0x10000, .host_features = 0xff, .max_stack_bytes = 65536};
    xip_slot_rules_t fixed = pic;
    fixed.fixed_vma = true; fixed.code_base = 0x40300000; fixed.data_base = 0x3f700000;

    xip_dmod_header h = good();
    CHECK(xip_dmod_check(&h, &pic) == EMD_OK);
    h.magic = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_MAGIC);
    h = good(); h.abi_version = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_VERSION);
    h = good(); h.abi_version = XIP_ABI_VERSION + 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_VERSION);
    h = good(); h.header_size = 32; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);
    h = good(); h.reserved[5] = 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);
    h = good(); h.manifest_offset = 0x1f0; h.manifest_size = 0x10; CHECK(xip_dmod_check(&h, &pic) == EMD_OK);     /* at the end */
    h = good(); h.manifest_offset = 0x1f0; h.manifest_size = 0x11; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);   /* past the end */
    h = good(); h.manifest_offset = 0x1f1; h.manifest_size = 4; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);      /* unaligned */
    h = good(); h.manifest_offset = 0x20; h.manifest_size = 4; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);      /* inside the header */
    h = good(); h.manifest_offset = 0x100; h.manifest_size = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);     /* an offset with no size */
    h = good(); h.manifest_size = 8; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);                                /* a size with no offset */
    h = good(); h.stack_bytes = 1u << 20; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);
    h = good(); h.image_size = 0x20000; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_SIZE);
    h = good(); h.image_size = 64; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_SIZE);
    h = good(); h.entry_offset = 0x10; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_ENTRY);
    h = good(); h.entry_offset = 0x200; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_ENTRY);
    h = good(); h.entry_offset = 0x45; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_ENTRY);
    h = good(); h.tag = 3; CHECK(xip_dmod_check(&h, &pic) == EMD_WRONG_TAG);
    h = good(); h.required_features = 0x1000; CHECK(xip_dmod_check(&h, &pic) == EMD_MISSING_FEATURE);
    h = good(); h.flags = XIP_DMOD_FLAG_FIXED_VMA; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);
    h = good(); h.code_vma = 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);

    h = good(); CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* PIC image on a fixed-address chip */
    h.flags = XIP_DMOD_FLAG_FIXED_VMA; h.code_vma = 0x40310000; h.data_vma = 0x3f710000;
    CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    h.code_vma = 0x40300000; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* another slot's window */
    h.code_vma = 0x40310000; h.data_vma = 0; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
}

/* package = public key (65) | signature (64) | header (216) | block table (8 per block) | image */
static void test_package(const char *path, int cpu, uint32_t slot_offset)
{
    FILE *f = fopen(path, "rb");
    CHECK(f != NULL);
    if (!f) return;
    static uint8_t pkg[1 << 20];
    size_t n = fread(pkg, 1, sizeof(pkg), f);
    fclose(f);
    CHECK(n > 345);
    uint16_t blocks;
    memcpy(&blocks, pkg + 65 + 64 + 18, 2);
    const uint8_t *image = pkg + 345 + (size_t)blocks * 8;
    /* the image inside is a module for slot tag 2 and passes the device's header check */
    xip_slot_rules_t rules = {.tag = 2, .slot_offset = slot_offset, .slot_size = 0x10000, .host_features = 0xff,
                                 .max_stack_bytes = 65536, .fixed_vma = cpu != 13,
                                 .code_base = cpu == 9 ? 0x43000000 : 0x40300000, .data_base = cpu == 9 ? 0x3d000000 : 0x3f700000};
    CHECK(xip_dmod_check((const xip_dmod_header *)image, &rules) == EMD_OK);
}

int main(int argc, char **argv)
{
    test_dmod();
    if (argc == 5) test_package(argv[1], atoi(argv[3]), (uint32_t)strtoul(argv[4], NULL, 0));
    puts(failures ? "FAILED" : "checks ok");
    return failures != 0;
}
