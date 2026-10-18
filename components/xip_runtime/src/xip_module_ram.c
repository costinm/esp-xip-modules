/* SPDX-License-Identifier: Apache-2.0 */
#include <stddef.h>
#include <string.h>
#include "heap_memory_layout.h"
#include "xip_module_ram.h"

#define ARENA_BYTES (XIP_DMOD_STATIC_SLOT_BYTES * XIP_DMOD_STATIC_SLOTS)
/* NOLOAD linker allocation protects Main; this descriptor protects SDK heaps. */
SOC_RESERVE_MEMORY_REGION(XIP_MODULE_RAM_BASE, XIP_MODULE_RAM_BASE + ARENA_BYTES, xip_module_ram);

void xip_module_ram_init(void)
{
    memset((void *)(uintptr_t)XIP_MODULE_RAM_BASE, 0, ARENA_BYTES);
}

bool xip_module_ram_clear(uint32_t address)
{
    if (address < XIP_MODULE_RAM_BASE || address - XIP_MODULE_RAM_BASE >= ARENA_BYTES ||
        (address - XIP_MODULE_RAM_BASE) % XIP_DMOD_STATIC_SLOT_BYTES) return false;
    memset((void *)(uintptr_t)address, 0, XIP_DMOD_STATIC_SLOT_BYTES);
    return true;
}
