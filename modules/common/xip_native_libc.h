/* SPDX-License-Identifier: Apache-2.0 */
/* The C library of a packed C module comes from the runtime (`get_component("native")`, xip_native in xip.h): the build passes this
 * header with -include. The module image has no .data/.bss and links no libc archive, and the table is in the firmware, whose address a
 * module image cannot know at build time. So the runtime writes it in word 1 of the module's RAM slot whenever it clears the slot
 * (xip_module_ram.c: the module's cell is at the start of the slot; word 0 is the module's own), and the code reads it from there.
 *   - `snprintf` is a macro (a variadic call has no trampoline in C);
 *   - the rest are real functions in xip_native_libc.c, because the compiler also emits calls to memcpy and memset itself.
 * Not in a packed image (XIP_CELL_ADDR unset: a host or linked build) this header does nothing and the process's libc is used. */
#pragma once
#ifdef XIP_CELL_ADDR
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "xip.h"

#define XIP_NATIVE() ((const xip_native *)((void *const *)(uintptr_t)(XIP_CELL_ADDR))[1])
#define snprintf(...) (XIP_NATIVE()->snprintf(__VA_ARGS__))
#endif
