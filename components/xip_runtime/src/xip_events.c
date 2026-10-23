/* SPDX-License-Identifier: Apache-2.0 */
/* The state of the Wi-Fi, heap_info and heap_dump, as CBOR records: the keys are the `wifi` record of modules/mod_con/API.md, which the console
 * module turns into text. Nothing here formats a string. */
#include <stdio.h>
#include <string.h>
#include "esp_heap_caps.h"
#include "esp_rom_sys.h"
#include "esp_log.h"
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#if CONFIG_HEAP_TRACING_STANDALONE
#include "esp_heap_trace.h"
#endif
#include <stdio.h>
#include "xip_cbor.h"
#include "xip_wifi.h"
#include "internal.h"

// TODO: rename to xip_debug.c

/* Two boot measurements, not retained allocation records. Replay them in the
 * binary snapshot header even when early ROM output used a different UART. */
static uint32_t entry_heap_total, entry_heap_free;

void xip_heap_checkpoint(const char *phase)
{
    /* Reading heap counters allocates nothing; transition output uses the
     * existing logger/retained pool leases once the product installs it. */
    const uint32_t total = heap_caps_get_total_size(MALLOC_CAP_8BIT);
    const uint32_t available = heap_caps_get_free_size(MALLOC_CAP_8BIT);
    if (!entry_heap_total) { entry_heap_total = total; entry_heap_free = available; }
    if (!strcmp(phase, "app-entry")) {
        esp_rom_printf("heap phase=%s total=%u free=%u\n", phase,
                       (unsigned)total, (unsigned)available);
    } else {
        /* Product firmware routes ESP_LOG into bounded CBOR history. Avoid
         * blocking the radio resume owner on physical UART output there. */
        ESP_LOGW("heap", "phase=%s total=%u free=%u", phase,
                 (unsigned)total, (unsigned)available);
    }
}

/* {1: state, 2: ssid, 3: ip, 4: rssi} (the last three once up), 5: AP up, 6: channel,
 * 7: the `now` record of api/host.API.md (the action-path counters) */
int xip_wifi_info(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap)
{
    xip_wifi_state st = {.size = sizeof(st)};
    int rc = hooks->wifi_state(&st);
    if (rc) return rc;
    xip_espnow_stats_t now;
    xip_link_espnow_stats(&now);
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    xip_cbor_map(&w, (st.sta == 2 ? 4 : 1) + (st.ap ? 1 : 0) + (st.channel ? 1 : 0) + 1);
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
    xip_cbor_uint(&w, 7);
    xip_cbor_map(&w, 14);
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, now.dispatcher);
    xip_cbor_uint(&w, 2); xip_cbor_uint(&w, now.tx_response_hook);
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, now.actions);
    xip_cbor_uint(&w, 4); xip_cbor_uint(&w, now.self_echoes);
    xip_cbor_uint(&w, 5); xip_cbor_uint(&w, now.duplicates);
    xip_cbor_uint(&w, 6); xip_cbor_uint(&w, now.invalid);
    xip_cbor_uint(&w, 7); xip_cbor_uint(&w, now.parse);
    xip_cbor_uint(&w, 8); xip_cbor_uint(&w, now.busy);
    xip_cbor_uint(&w, 9); xip_cbor_uint(&w, now.drops);
    xip_cbor_uint(&w, 10); xip_cbor_uint(&w, now.tx);
    xip_cbor_uint(&w, 11); xip_cbor_uint(&w, now.tx_failures);
    xip_cbor_uint(&w, 12); xip_cbor_int(&w, now.tx_last_error);
    xip_cbor_uint(&w, 13); xip_cbor_uint(&w, now.hook_calls);
    xip_cbor_uint(&w, 14); xip_cbor_uint(&w, now.disabled);
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

/* BEGIN HEAP BLOCK SNAPSHOT: no allocation or output inside allocator locks. */
typedef struct {
    intptr_t heap;
    void *address;
    size_t size;
    bool used;
} heap_block_snapshot_t;
typedef struct {
    heap_block_snapshot_t *blocks;
    size_t count, seen, capacity;
} heap_snapshot_t;

static bool capture_heap_block(walker_heap_into_t heap, walker_block_info_t block, void *ctx)
{
    heap_snapshot_t *snapshot = ctx;
    ++snapshot->seen;
    if (snapshot->count < snapshot->capacity)
        snapshot->blocks[snapshot->count++] = (heap_block_snapshot_t){heap.start, block.ptr, block.size, block.used};
    return true;
}

static int heap_dump_blocks(bool binary)
{
    /* Temporary diagnostic memory, released after output. The snapshot contains
     * its own allocation; report its address so tools can exclude that block. */
    const size_t capacity = 1024;
    heap_block_snapshot_t *blocks = heap_caps_malloc(capacity * sizeof(*blocks), MALLOC_CAP_8BIT);
    if (!blocks) return XIP_ERR_NOMEM;
    heap_snapshot_t snapshot = {.blocks = blocks, .capacity = capacity};
    heap_caps_walk(MALLOC_CAP_8BIT, capture_heap_block, &snapshot);
    int rc = XIP_OK;
    if (binary) {
        /* PPP diagnostic records on the console port. Type 0 header, 1 block,
         * 2 end. No pointer is dereferenced after the allocator lock is gone. */
        uint8_t data[96];
        xip_cbor_w w;
        xip_cbor_init(&w, data, sizeof(data));
        xip_cbor_map(&w, 8);
        xip_cbor_uint(&w, 0); xip_cbor_uint(&w, 0);
        xip_cbor_uint(&w, 1); xip_cbor_uint(&w, snapshot.count);
        xip_cbor_uint(&w, 2); xip_cbor_uint(&w, snapshot.seen);
        xip_cbor_uint(&w, 3); xip_cbor_uint(&w, (uintptr_t)blocks);
        xip_cbor_uint(&w, 4); xip_cbor_uint(&w, capacity * sizeof(*blocks));
        xip_cbor_uint(&w, 5); xip_cbor_bool(&w, snapshot.seen > snapshot.count);
        xip_cbor_uint(&w, 6); xip_cbor_uint(&w, entry_heap_total);
        xip_cbor_uint(&w, 7); xip_cbor_uint(&w, entry_heap_free);
        rc = xip_serial_write_frame(XIP_PORT_CONSOLE, data, xip_cbor_done(&w));
        for (size_t i = 0; !rc && i < snapshot.count; ++i) {
            const heap_block_snapshot_t *block = &blocks[i];
            xip_cbor_init(&w, data, sizeof(data));
            xip_cbor_map(&w, 5);
            xip_cbor_uint(&w, 0); xip_cbor_uint(&w, 1);
            xip_cbor_uint(&w, 1); xip_cbor_uint(&w, block->heap);
            xip_cbor_uint(&w, 2); xip_cbor_uint(&w, (uintptr_t)block->address);
            xip_cbor_uint(&w, 3); xip_cbor_uint(&w, block->size);
            xip_cbor_uint(&w, 4); xip_cbor_bool(&w, block->used);
            rc = xip_serial_write_frame(XIP_PORT_CONSOLE, data, xip_cbor_done(&w));
        }
        if (!rc) {
            xip_cbor_init(&w, data, sizeof(data));
            xip_cbor_map(&w, 2);
            xip_cbor_uint(&w, 0); xip_cbor_uint(&w, 2);
            xip_cbor_uint(&w, 1); xip_cbor_uint(&w, snapshot.count);
            rc = xip_serial_write_frame(XIP_PORT_CONSOLE, data, xip_cbor_done(&w));
        }
    } else {
        printf("Heap snapshot: %u of %u blocks, buffer %p (%u bytes), truncated %u\n",
               (unsigned)snapshot.count, (unsigned)snapshot.seen, (void *)blocks,
               (unsigned)(capacity * sizeof(*blocks)), snapshot.seen > snapshot.count);
        for (size_t i = 0; i < snapshot.count; ++i) {
            const heap_block_snapshot_t *block = &blocks[i];
            printf("heap 0x%lx block %p size %u used %u\n", (unsigned long)block->heap,
                   block->address, (unsigned)block->size, block->used);
        }
    }
    heap_caps_free(blocks);
    return rc ? rc : snapshot.seen > snapshot.count ? XIP_ERR_SIZE : XIP_OK;
}
/* END HEAP BLOCK SNAPSHOT */

int xip_heap_dump(int kind)
{
    switch (kind) {
    case 0: heap_caps_print_heap_info(MALLOC_CAP_8BIT); return XIP_OK;
    case 1: return heap_dump_blocks(false);
    case 6: return heap_dump_blocks(true);
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
