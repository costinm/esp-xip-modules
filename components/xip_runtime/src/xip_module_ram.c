/* SPDX-License-Identifier: Apache-2.0 */
#include <stddef.h>
#include <string.h>
#include "heap_memory_layout.h"
#include "xip_module_ram.h"

#define ARENA_BYTES (XIP_DMOD_STATIC_SLOT_BYTES * XIP_DMOD_STATIC_SLOTS)
/* NOLOAD linker allocation protects Main; this descriptor protects SDK heaps. */
SOC_RESERVE_MEMORY_REGION(XIP_MODULE_RAM_BASE, XIP_MODULE_RAM_BASE + ARENA_BYTES, xip_module_ram);

extern const xip_native *xip_native_component(void);

/* The first words of a module's RAM slot belong to the runtime to describe the runtime to the module, at fixed offsets, so that an
 * image that has no .data/.bss and cannot be given an address at build time finds them (modules/common/xip_native_libc.h):
 *   word 0  the module's own cell (state pointer of a C module, current host of a Rust allocator): zero here
 *   word 1  the `native` table (xip_native), const data of the runtime
 *   word 2, 3  reserved (a module context)
 * Written whenever a slot is cleared, so a module can rely on them from its first instruction. */
static void describe(uint32_t address)
{
    ((const void **)(uintptr_t)address)[1] = xip_native_component();
}

void xip_module_ram_init(void)
{
    memset((void *)(uintptr_t)XIP_MODULE_RAM_BASE, 0, ARENA_BYTES);
    for (uint32_t slot = 0; slot < XIP_DMOD_STATIC_SLOTS; slot++)
        describe(XIP_MODULE_RAM_BASE + slot * XIP_DMOD_STATIC_SLOT_BYTES);
}

bool xip_module_ram_clear(uint32_t address)
{
    if (address < XIP_MODULE_RAM_BASE || address - XIP_MODULE_RAM_BASE >= ARENA_BYTES ||
        (address - XIP_MODULE_RAM_BASE) % XIP_DMOD_STATIC_SLOT_BYTES) return false;
    memset((void *)(uintptr_t)address, 0, XIP_DMOD_STATIC_SLOT_BYTES);
    describe(address);
    return true;
}
