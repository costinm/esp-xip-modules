/* SPDX-License-Identifier: Apache-2.0 */
/* The state of the Wi-Fi, heap_info and heap_dump, as CBOR records: the keys are the `wifi` record of modules/mod_con/API.md, which the console
 * module turns into text. Nothing here formats a string. */
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
