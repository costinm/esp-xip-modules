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
    xip_dmod_header h = {.kind = XIP_DMOD_KIND_MODULE, .abi_version = XIP_ABI_VERSION, .header_size = 64,
                            .entry_offset = 0x44, .image_size = 0x200, .tag = 2,
                            .flags = XIP_DMOD_FLAG_STATIC_RAM, .static_addr = XIP_DMOD_STATIC_ESP32C6_BASE + 16};
    return h;
}

static void test_dmod(void)
{
    xip_slot_rules_t pic = {.slot_offset = 0x10000, .slot_size = 0x10000, .max_stack_bytes = 65536, .static_base = XIP_DMOD_STATIC_ESP32C6_BASE};
    xip_slot_rules_t fixed = pic;
    fixed.fixed_vma = true; fixed.static_base = XIP_DMOD_STATIC_ESP32_BASE; fixed.code_base = 0x40250000; fixed.data_base = 0x3f650000; fixed.window_bytes = 27 * XIP_DMOD_WINDOW_SLOT_BYTES; /* the classic ESP32: 1.5 MiB reserve to SOC_IROM_HIGH */

    xip_dmod_header h = good();
    CHECK(xip_dmod_check(&h, &pic) == EMD_OK);
    h.kind = 0; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_MAGIC);
    h = good(); h.kind = 'D'; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_MAGIC);        /* a data extent is not a module */
    h = good(); h.reserved1 = 1; CHECK(xip_dmod_check(&h, &pic) == EMD_BAD_HEADER);
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
    CHECK(offsetof(xip_dmod_header, image_size) == 4 && offsetof(xip_dmod_header, entry_offset) == 8);   /* the 8-byte lead the scan reads */

    h = good(); CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* PIC image on a fixed-address chip */
    h.flags = XIP_DMOD_FLAG_FIXED_VMA | XIP_DMOD_FLAG_STATIC_RAM; h.static_addr = fixed.static_base + 16; h.code_vma = 0x40260000; h.data_vma = 0x3f660000;
    CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    h.code_vma = 0x40250000; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* another module's window slot */
    h.code_vma = 0x40260000; h.data_vma = 0; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
    h.data_vma = 0x3f670000; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
    /* the link address is the module's, the flash page is not: any page, same result */
    h.data_vma = 0x3f660000;
    for (uint32_t page = 0; page < 8; page++) {
        fixed.slot_offset = page * 0x10000; fixed.slot_size = 0x10000;
        CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    }
    h.code_vma = 0x40260001; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* not page aligned */
    h.code_vma = 0x40260000;
    /* Multi-page modules are bounded by physical room and the entire virtual span. */
    h.image_size=0x10004; fixed.slot_size=0x20000;
    CHECK(xip_dmod_check(&h,&fixed)==EMD_OK);
    fixed.slot_size=0x10000; CHECK(xip_dmod_check(&h,&fixed)==EMD_BAD_SIZE);
    fixed.slot_size=0x20000; fixed.window_bytes = 16 * XIP_DMOD_WINDOW_SLOT_BYTES;   /* the end of the window bounds the span */
    h.static_addr=fixed.static_base+14*XIP_DMOD_STATIC_SLOT_BYTES;
    h.code_vma=fixed.code_base+14*XIP_DMOD_WINDOW_SLOT_BYTES;
    h.data_vma=fixed.data_base+14*XIP_DMOD_WINDOW_SLOT_BYTES;
    CHECK(xip_dmod_check(&h,&fixed)==EMD_OK);
    h.static_addr+=XIP_DMOD_STATIC_SLOT_BYTES;h.code_vma+=XIP_DMOD_WINDOW_SLOT_BYTES;h.data_vma+=XIP_DMOD_WINDOW_SLOT_BYTES;
    CHECK(xip_dmod_check(&h,&fixed)==EMD_BAD_SIZE); /* tail crosses window */
    h.image_size=0x200;fixed.slot_size=0x10000;
    /* the slot the header declares is its cell: cell 15 is the last one, vma outside the window is refused */
    h.static_addr = fixed.static_base + 15 * XIP_DMOD_STATIC_SLOT_BYTES; h.code_vma = 0x40250000 + 15 * 0x10000; h.data_vma = 0x3f650000 + 15 * 0x10000;
    CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    fixed.window_bytes = 0x100000; CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    fixed.window_bytes = 0x80000; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);   /* a smaller window does not hold slot 15 */
#define IN_SLOT(n) do { h.static_addr = fixed.static_base + (n) * XIP_DMOD_STATIC_SLOT_BYTES; h.code_vma = fixed.code_base + (n) * 0x10000; h.data_vma = fixed.data_base + (n) * 0x10000; } while (0)
    fixed.window_bytes = 16 * XIP_DMOD_WINDOW_SLOT_BYTES;      /* a 16-window chip refuses slot 16 */
    IN_SLOT(16); CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
    fixed.window_bytes = 27 * XIP_DMOD_WINDOW_SLOT_BYTES;      /* the classic ESP32: slots 16 and 26 exist, 27 does not */
    IN_SLOT(16); CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    IN_SLOT(26); CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    IN_SLOT(27); CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_PLACEMENT);
    fixed.window_bytes = 16 * XIP_DMOD_WINDOW_SLOT_BYTES;
    IN_SLOT(15);
    h.image_size = 0x10000; CHECK(xip_dmod_check(&h, &fixed) == EMD_OK);
    fixed.slot_size = 0x20000; h.image_size = 0x10004; CHECK(xip_dmod_check(&h, &fixed) == EMD_BAD_SIZE);   /* longer than its window slot */
}

int main(void)
{
    test_dmod();
    puts(failures ? "FAILED" : "checks ok");
    return failures != 0;
}
