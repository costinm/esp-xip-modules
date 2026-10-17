/* SPDX-License-Identifier: Apache-2.0 */
#include <stdlib.h>
#include <string.h>
#include "esp_chip_info.h"
#include "esp_flash.h"
#include "esp_cpu.h"
#include "esp_heap_caps.h"
#include "esp_ota_ops.h"
#include "bootloader_common.h"
#include "xip_cbor.h"
#include "internal.h"

static SemaphoreHandle_t info_mutex;
#define INFO_LOCK() xSemaphoreTake(info_mutex, portMAX_DELAY)
#define INFO_UNLOCK() xSemaphoreGive(info_mutex)
static size_t info_memory_available(void) { return heap_caps_get_free_size(MALLOC_CAP_8BIT); }
#include "info_cache.inc"

int xip_info_init(void)
{
    if (!info_mutex) info_mutex = xSemaphoreCreateMutex();
    return info_mutex ? XIP_OK : XIP_ERR_NOMEM;
}
int xip_rt_info_publish(uint32_t owner, int32_t component, int32_t method, const uint8_t *data, size_t len)
{
    if (!info_mutex) return XIP_ERR_UNSUPPORTED;
    return info_publish(owner, component, method, data, len, (uint64_t)(esp_timer_get_time() / 1000));
}
int xip_info_publish(int32_t component, int32_t method, const uint8_t *data, size_t len)
{
    return xip_rt_info_publish(0, component, method, data, len);
}
void xip_info_release(uint32_t owner) { if (info_mutex && owner) info_release(owner); }

/* Stage2's SDK boot path can retain the actual loaded partition, including a
 * fallback, in the existing CRC-protected RTC record. Older Stage2 builds leave
 * it unset. Cross-check against the running image so stale retained data cannot
 * misreport the partition after a bootloader update or different boot path. */
static const esp_partition_t *running_partition(void)
{
    const esp_partition_t *image = esp_ota_get_running_partition();
#if CONFIG_BOOTLOADER_RESERVE_RTC_MEM
    /* ESP32 RTC FAST memory is accessible only on PRO_CPU. Other callers
     * retain the running-image fallback instead of reading it from APP_CPU. */
    if (esp_cpu_get_core_id() != 0) return image;
    const esp_partition_pos_t *rtc = bootloader_common_get_rtc_retain_mem_partition();
    if (rtc && rtc->size && (!image || image->address == rtc->offset)) {
        esp_partition_iterator_t it = esp_partition_find(ESP_PARTITION_TYPE_APP,
                                                         ESP_PARTITION_SUBTYPE_ANY, NULL);
        while (it) {
            const esp_partition_t *part = esp_partition_get(it);
            if (part->address == rtc->offset && part->size == rtc->size) {
                esp_partition_iterator_release(it);
                return part;
            }
            it = esp_partition_next(it);
        }
    }
#endif
    return image;
}

int xip_rt_device_info(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap)
{
    if (!out || !cap) return XIP_ERR_ARG;
    if (!info_mutex) return XIP_ERR_UNSUPPORTED;
    xip_cbor_w w; xip_cbor_init(&w, out, cap);
    esp_chip_info_t chip; esp_chip_info(&chip);
    uint32_t flash_size = 0;
    if (esp_flash_get_size(NULL, &flash_size) != ESP_OK) return XIP_ERR_IO;
    const esp_partition_t *part = running_partition();
    xip_cbor_map(&w, 5);
    xip_cbor_uint(&w, 1); xip_cbor_map(&w, part ? 6 : 3);
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, (uint64_t)(esp_timer_get_time() / 1000));
    xip_cbor_uint(&w, 2); xip_cbor_uint(&w, chip.model);
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, flash_size);
    if (part) {
        xip_cbor_uint(&w, 4); xip_cbor_text(&w, part->label, strlen(part->label));
        xip_cbor_uint(&w, 5); xip_cbor_uint(&w, part->address);
        xip_cbor_uint(&w, 6); xip_cbor_uint(&w, part->size);
    }
    /* Let each existing encoder use the unused tail of the caller's buffer. */
    xip_cbor_uint(&w, 2);
    if (w.overflow) return XIP_ERR_SIZE;
    int n = hooks->wifi_state ? xip_wifi_info(hooks, out + w.len, cap - w.len) : XIP_ERR_UNSUPPORTED;
    if (n == XIP_ERR_UNSUPPORTED) xip_cbor_map(&w, 0);
    else if (n < 0) return n; else w.len += (size_t)n;
    xip_cbor_uint(&w, 3);
    if (w.overflow) return XIP_ERR_SIZE;
    n = xip_heap_info(out + w.len, cap - w.len);
    if (n < 0) return n;
    w.len += (size_t)n;
    xip_cbor_uint(&w, 4);
    if (w.overflow) return XIP_ERR_SIZE;
    n = hooks->modules_info ? hooks->modules_info(out + w.len, cap - w.len) : XIP_ERR_UNSUPPORTED;
    if (n == XIP_ERR_UNSUPPORTED) xip_cbor_array(&w, 0);
    else if (n < 0) return n; else w.len += (size_t)n;
    xip_cbor_uint(&w, 5); info_components(&w);
    return xip_cbor_done(&w);
}
