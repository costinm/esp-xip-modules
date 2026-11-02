/* SPDX-License-Identifier: Apache-2.0 */
/* get_component("native"): the C library functions modules call (xip_native, include/xip.h). `const` data in the runtime's flash.
 * Each entry is the plain libc symbol, so it is the chip's ROM function where the ESP-IDF linker scripts give one (esp_rom: memcpy,
 * memmove, memset, memcmp, strlen, strcmp, ...) and the firmware's newlib otherwise: nothing is duplicated in a module. snprintf is
 * the runtime's formatter (xip_fmt.c). Only functions that neither allocate for the module nor remember the caller belong here. */
#include <math.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include "xip.h"
#include "xip_fmt.h"

static const xip_native native = {
    .version = 1,
    .size = sizeof(xip_native),
    .memcpy = memcpy, .memmove = memmove, .memset = memset, .memcmp = memcmp,
    .strlen = strlen, .strcmp = strcmp, .strncmp = strncmp, .strcpy = strcpy, .strncpy = strncpy,
    .strcat = strcat, .strchr = strchr, .strstr = strstr,
    .strtod = strtod, .strtol = strtol, .strtoul = strtoul,
    .snprintf = xip_fmt_snprintf,
    .fmod = fmod, .floor = floor, .ceil = ceil, .trunc = trunc, .rint = rint, .sqrt = sqrt, .fabs = fabs, .copysign = copysign,
    .floorf = floorf, .ceilf = ceilf, .truncf = truncf, .rintf = rintf, .sqrtf = sqrtf, .fabsf = fabsf, .copysignf = copysignf,
};

const xip_native *xip_native_component(void)
{
    return &native;
}
