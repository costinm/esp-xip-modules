#pragma once

/* RTC handoff contract shared by Stage2 (bootloader), the loader and Main.
 * Header-only and free of app-component dependencies so Stage2 can include it
 * by relative path. Byte offsets are documented in docs/app-integration.md; a
 * Main in any language may write the handoff byte by hand. */

#include <stdint.h>
#include "sdkconfig.h"
#include "soc/soc.h"

/* One-shot boot request, written by Main/loader, consumed by Stage2. */
#define WL_HANDOFF_NORMAL 0u
#define WL_HANDOFF_LOADER 1u /* next boot: run the loader (upgrade request) */
#define WL_HANDOFF_MAIN 2u   /* next boot: run Main (loader finished) */

/* Why Stage2 selected the loader (written by Stage2, read by the loader). */
#define WL_REASON_NONE 0u
#define WL_REASON_CRASH 1u  /* reset pattern or abnormal-reset loop */
#define WL_REASON_UPDATE 2u /* Main's request or the entry button */

/* Stage2-owned flash read-mode trial bits (WL_RTC_FLASH). */
#define WL_FLASH_QIO_TRIAL 0x01u /* Main was booted in QIO, outcome pending */
#define WL_FLASH_DIO_TRIAL 0x02u /* QIO trial failed; same Main retried in DIO */
#define WL_FLASH_QIO_ACTIVE 0x04u /* the current boot reads code in QIO */

/* Layout: 12 fixed bytes of rtc_retain_mem_t, 32 custom bytes, CRC. */
#define WL_RTC_RETAIN_RAW_SIZE (((12 + 0x20u + 4 + 7) / 8) * 8)
#define WL_RTC_CUSTOM_OFFSET 12
#define WL_RTC_HANDOFF_OFFSET (WL_RTC_CUSTOM_OFFSET + 5)
#define WL_RTC_FLASH_OFFSET (WL_RTC_CUSTOM_OFFSET + 6)
#define WL_RTC_REASON_OFFSET (WL_RTC_CUSTOM_OFFSET + 7)

#if CONFIG_IDF_TARGET_ESP32C6
/* C6 application code cannot write SOC_RTC_DRAM_LOW; use the writable
 * high-end block. */
#define WL_RTC_RETAIN_BASE (SOC_RTC_DRAM_HIGH - WL_RTC_RETAIN_RAW_SIZE)
#elif ESP_ROM_HAS_LP_ROM
#define WL_RTC_RETAIN_BASE SOC_RTC_DRAM_LOW
#else
#define WL_RTC_RETAIN_BASE (SOC_RTC_DRAM_HIGH - WL_RTC_RETAIN_RAW_SIZE)
#endif

#define WL_RTC_HANDOFF (*(volatile uint8_t *)(WL_RTC_RETAIN_BASE + WL_RTC_HANDOFF_OFFSET))
#define WL_RTC_FLASH (*(volatile uint8_t *)(WL_RTC_RETAIN_BASE + WL_RTC_FLASH_OFFSET))
#define WL_RTC_REASON (*(volatile uint8_t *)(WL_RTC_RETAIN_BASE + WL_RTC_REASON_OFFSET))
