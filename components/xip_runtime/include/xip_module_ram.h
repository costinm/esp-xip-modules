/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "sdkconfig.h"
#include "xip.h"

#if CONFIG_IDF_TARGET_ESP32
#define XIP_MODULE_RAM_BASE XIP_DMOD_STATIC_ESP32_BASE
#elif CONFIG_IDF_TARGET_ESP32S3
#define XIP_MODULE_RAM_BASE XIP_DMOD_STATIC_ESP32S3_BASE
#elif CONFIG_IDF_TARGET_ESP32C6
#define XIP_MODULE_RAM_BASE XIP_DMOD_STATIC_ESP32C6_BASE
#else
#error "Define and reserve the module RAM arena for this target"
#endif

/* Called once before scanning, then only for a stopped, exclusively claimed cell. */
void xip_module_ram_init(void);
bool xip_module_ram_clear(uint32_t address);
