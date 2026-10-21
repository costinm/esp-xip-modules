/* SPDX-License-Identifier: Apache-2.0 */
/* The checks a device makes before mapping or installing a module (components/xip_loader/src/xip_check.c).
 * The package around an image on the upload port is verified by modules/mod_flash. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xip_check.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static xip_dmod_header good(void)
{
    xip_dmod_header h = {.magic = XIP_DMOD_MAGIC, .abi_version = XIP_ABI_VERSION, .header_size = 64,
                            .entry_offset = 0x44, .image_size = 0x200, .tag = 2,
                            .flags = XIP_DMOD_FLAG_STATIC_RAM, .static_addr = XIP_DMOD_STATIC_ESP32C6_BASE + 16};
    return h;
}

static void test_dmod(void)
{
    xip_slot_rules_t pic = {.slot_offset = 0x10000, .slot_size = 0x10000, .max_stack_bytes = 65536, .static_base = XIP_DMOD_STATIC_ESP32C6_BASE};
    xip_slot_rules_t fixed = pic;
    fixed.fixed_vma = true; fixed.static_base = XIP_DMOD_STATIC_ESP32_BASE; fixed.code_base = 0x40300000; fixed.data_base = 0x3f700000;

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
    h = good(); h.tag = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);          /* the component number is the image's own */
    h = good(); h.required_features = 0x1000; CHECK(xip_dmod_check(&h, &pic) == EMD_OK);  /* reserved: not part of RAM placement */
    h = good(); h.flags = XIP_DMOD_FLAG_FIXED_VMA | XIP_DMOD_FLAG_STATIC_RAM; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);
    h = good(); h.code_vma = 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);

    h = good(); h.flags = 0; h.static_addr = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT); /* old image */
    h = good(); h.flags |= 4; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT); /* unknown flags */
    h = good(); h.static_addr = pic.static_base - 16; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);
    h = good(); h.static_addr = pic.static_base + XIP_DMOD_STATIC_SLOTS * XIP_DMOD_STATIC_SLOT_BYTES; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);
    h = good(); h.static_addr = pic.static_base + 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT);
    h = good(); h.static_addr = XIP_DMOD_STATIC_ESP32_BASE; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_PLACEMENT); /* wrong platform */
    h = good(); h.static_addr = pic.static_base + 15 * XIP_DMOD_STATIC_SLOT_BYTES; CHECK(xip_dmod_check(&h, &pic) == EMD_OK);
    pic.slot_offset = 0x90000; CHECK(xip_dmod_check(&h, &pic) == EMD_OK); /* PIC flash placement does not rename RAM cell */
    pic.slot_offset = 0x10000;
    CHECK(offsetof(xip_dmod_header, static_addr) == 52 && offsetof(xip_dmod_header, reserved) == 56);

    h = good(); CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* PIC image on a fixed-address chip */
    h.flags = XIP_DMOD_FLAG_FIXED_VMA | XIP_DMOD_FLAG_STATIC_RAM; h.static_addr = fixed.static_base + 16; h.code_vma = 0x40310000; h.data_vma = 0x3f710000;
    CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    h.code_vma = 0x40300000; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* another slot's window */
    h.code_vma = 0x40310000; h.data_vma = 0; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
}

int main(void)
{
    test_dmod();
    puts(failures ? "FAILED" : "checks ok");
    return failures != 0;
}
