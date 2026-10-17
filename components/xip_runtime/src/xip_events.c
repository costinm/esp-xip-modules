/* SPDX-License-Identifier: Apache-2.0 */
/* The last events modules emitted (events_info) and the state of the Wi-Fi (wifi_info), as CBOR records: the keys are the
 * `event` and `wifi` records of modules/mod_console/API.md, which the console module turns into text. Nothing here
 * formats a string. */
#include <stdio.h>
#include <string.h>
#include "esp_heap_caps.h"
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#if CONFIG_HEAP_TRACING_STANDALONE
#include "esp_heap_trace.h"
#endif
#include <stdio.h>
#include "xip_cbor.h"
#include "internal.h"

#define EVENTS 16
#define VALUE_KEEP 32

static struct { char module[16]; uint16_t id; uint8_t type, len; uint8_t data[VALUE_KEEP]; } ring[EVENTS];
static unsigned ring_next;

void xip_events_add(const char *module, uint16_t id, uint8_t type, const uint8_t *value, size_t len)
{
    unsigned i = ring_next++ % EVENTS;
    strlcpy(ring[i].module, module, sizeof(ring[i].module));
    ring[i].id = id;
    ring[i].type = type;
    ring[i].len = len > VALUE_KEEP ? VALUE_KEEP : (uint8_t)len;
    if (ring[i].len) memcpy(ring[i].data, value, ring[i].len);
}

/* {1: module, 2: id, 3: value as text (a text value as it is, anything else as hex)} per event, oldest first */
int xip_events_info(uint8_t *out, size_t cap)
{
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    unsigned count = ring_next < EVENTS ? ring_next : EVENTS;
    xip_cbor_array(&w, count);
    for (unsigned k = 0; k < count; k++) {
        unsigned i = (ring_next - count + k) % EVENTS;
        char hex[2 * VALUE_KEEP];
        const char *text = (const char *)ring[i].data;
        size_t n = ring[i].len;
        if (ring[i].type != XIP_VALUE_TEXT) {
            for (size_t b = 0; b < n; b++) { hex[2 * b] = "0123456789abcdef"[ring[i].data[b] >> 4]; hex[2 * b + 1] = "0123456789abcdef"[ring[i].data[b] & 15]; }
            text = hex;
            n *= 2;
        }
        xip_cbor_map(&w, 3);
        xip_cbor_uint(&w, 1); xip_cbor_text(&w, ring[i].module, strlen(ring[i].module));
        xip_cbor_uint(&w, 2); xip_cbor_uint(&w, ring[i].id);
        xip_cbor_uint(&w, 3); xip_cbor_text(&w, text, n);
    }
    int len = xip_cbor_done(&w);
    return len < 0 ? XIP_ERR_SIZE : len;
}

/* {1: state, 2: ssid, 3: ip, 4: rssi} (the last three once up), 5: AP up, 6: channel */
int xip_wifi_info(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap)
{
    xip_wifi_state st = {.size = sizeof(st)};
    int rc = hooks->wifi_state(&st);
    if (rc) return rc;
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    xip_cbor_map(&w, (st.sta == 2 ? 4 : 1) + (st.ap ? 1 : 0) + (st.channel ? 1 : 0));
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, st.sta);
    if (st.sta == 2) {
        char ip[16];
        int n = snprintf(ip, sizeof(ip), "%u.%u.%u.%u", st.ip4[0], st.ip4[1], st.ip4[2], st.ip4[3]);
        xip_cbor_uint(&w, 2); xip_cbor_text(&w, (const char *)st.ssid, st.ssid_len);
        xip_cbor_uint(&w, 3); xip_cbor_text(&w, ip, n > 0 ? (size_t)n : 0);
        xip_cbor_uint(&w, 4); xip_cbor_int(&w, st.rssi);
    }
    if (st.ap) { xip_cbor_uint(&w, 5); xip_cbor_bool(&w, true); }
    if (st.channel) { xip_cbor_uint(&w, 6); xip_cbor_uint(&w, st.channel); }
    int len = xip_cbor_done(&w);
    return len < 0 ? XIP_ERR_SIZE : len;
}

/* wifi_scan: {1: [{1: ssid, 2: bssid, 3: rssi, 4: channel, 5: auth}, ...]}, as many as fit (api/wifi.API.md) */
int xip_wifi_scan(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap)
{
    enum { MAX_APS = 24, ENTRY = 56 };
    xip_ap_t *aps = malloc(MAX_APS * sizeof(*aps));
    if (!aps) return XIP_ERR_NOMEM;
    int n = hooks->wifi_scan(aps, MAX_APS), rc;
    if (n < 0) rc = n == -5 ? XIP_ERR_BUSY : n == -6 ? XIP_ERR_NOMEM : XIP_ERR_IO;
    else {
        if ((size_t)n > (cap > 6 ? (cap - 6) / ENTRY : 0)) n = (int)(cap > 6 ? (cap - 6) / ENTRY : 0);
        xip_cbor_w w;
        xip_cbor_init(&w, out, cap);
        xip_cbor_map(&w, 1);
        xip_cbor_uint(&w, 1);
        xip_cbor_array(&w, (uint64_t)n);
        for (int i = 0; i < n; ++i) {
            xip_cbor_map(&w, 5);
            xip_cbor_uint(&w, 1); xip_cbor_bytes(&w, aps[i].ssid, aps[i].ssid_len);
            xip_cbor_uint(&w, 2); xip_cbor_bytes(&w, aps[i].bssid, 6);
            xip_cbor_uint(&w, 3); xip_cbor_int(&w, aps[i].rssi);
            xip_cbor_uint(&w, 4); xip_cbor_uint(&w, aps[i].channel);
            xip_cbor_uint(&w, 5); xip_cbor_uint(&w, aps[i].auth);
        }
        int len = xip_cbor_done(&w);
        rc = len < 0 ? XIP_ERR_SIZE : len;
    }
    free(aps);
    return rc;
}

/* heap_info: {1: free, 2: largest block, 3: lowest free since boot, 4: total, 5: free internal, 6: largest internal block} */
int xip_heap_info(uint8_t *out, size_t cap)
{
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    xip_cbor_map(&w, 6);
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, heap_caps_get_free_size(MALLOC_CAP_8BIT));
    xip_cbor_uint(&w, 2); xip_cbor_uint(&w, heap_caps_get_largest_free_block(MALLOC_CAP_8BIT));
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, heap_caps_get_minimum_free_size(MALLOC_CAP_8BIT));
    xip_cbor_uint(&w, 4); xip_cbor_uint(&w, heap_caps_get_total_size(MALLOC_CAP_8BIT));
    xip_cbor_uint(&w, 5); xip_cbor_uint(&w, heap_caps_get_free_size(MALLOC_CAP_8BIT | MALLOC_CAP_INTERNAL));
    xip_cbor_uint(&w, 6); xip_cbor_uint(&w, heap_caps_get_largest_free_block(MALLOC_CAP_8BIT | MALLOC_CAP_INTERNAL));
    int len = xip_cbor_done(&w);
    return len < 0 ? XIP_ERR_SIZE : len;
}

/* heap_dump: kinds in include/xip.h (heap_dump). Printed with the log, for a person at the port. */
#if CONFIG_HEAP_TRACING_STANDALONE
#define TRACE_RECORDS 200
static heap_trace_record_t *trace_buf;
#endif

int xip_heap_dump(int kind)
{
    switch (kind) {
    case 0: heap_caps_print_heap_info(MALLOC_CAP_8BIT); return XIP_OK;
    case 1: heap_caps_dump(MALLOC_CAP_8BIT); return XIP_OK;
    case 2:
#if CONFIG_FREERTOS_USE_TRACE_FACILITY
    {
        UBaseType_t n = uxTaskGetNumberOfTasks();
        TaskStatus_t *t = malloc(n * sizeof(*t));
        if (!t) return XIP_ERR_NOMEM;
        n = uxTaskGetSystemState(t, n, NULL);
        for (UBaseType_t i = 0; i < n; i++)
            printf("task %-16s prio %2u stack_left %u\n", t[i].pcTaskName, (unsigned)t[i].uxCurrentPriority, (unsigned)t[i].usStackHighWaterMark)   /* bytes in ESP-IDF */;
        free(t);
        return XIP_OK;
    }
#else
        return XIP_ERR_UNSUPPORTED;
#endif
#if CONFIG_HEAP_TRACING_STANDALONE
    case 3:
        if (!trace_buf) trace_buf = heap_caps_calloc(TRACE_RECORDS, sizeof(heap_trace_record_t), MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
        if (!trace_buf) return XIP_ERR_NOMEM;
        if (heap_trace_init_standalone(trace_buf, TRACE_RECORDS) != ESP_OK) return XIP_ERR_BUSY;
        return heap_trace_start(HEAP_TRACE_LEAKS) == ESP_OK ? XIP_OK : XIP_ERR_BUSY;
    case 4: heap_trace_dump(); return XIP_OK;
    case 5: heap_trace_stop(); return XIP_OK;
#else
    case 3: case 4: case 5: return XIP_ERR_UNSUPPORTED;
#endif
    default: return XIP_ERR_ARG;
    }
}
