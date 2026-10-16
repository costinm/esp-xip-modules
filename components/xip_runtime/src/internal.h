/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "sdkconfig.h"
#include "xip_pool.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "esp_timer.h"
#include "xip_runtime.h"

#define XIP_IRQ_SLOTS 8
#define XIP_SPI_DEVICES 4
#define XIP_ALARMS 8
#define XIP_MAX_INSTANCES 8

struct xip_instance;
typedef struct {
    struct xip_instance *inst;
    int pin;
    int32_t component, method;
    volatile bool used;
} xip_irq_slot_t;

typedef struct {
    void *handle;                 /* spi_device_handle_t */
    int host;                     /* spi_host_device_t */
    int ready_pin, ready_level;
    uint32_t ready_timeout_ms;
    uint8_t *tx, *rx;             /* DMA-capable bounce buffers */
    void *trans;                  /* spi_transaction_t: lives as long as the driver may still hold it */
    bool pending;                 /* queued, result not yet reclaimed (a timed-out transfer) */
    bool used;
} xip_spi_dev_t;

void xip_periph_reserve(uint64_t mask);
/* xip_events.c: the last events modules emitted, and the CBOR of events_info and wifi_info */
void xip_events_add(const char *module, uint16_t id, uint8_t type, const uint8_t *value, size_t len);
int xip_events_info(uint8_t *out, size_t cap);
int xip_wifi_info(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap);
int xip_heap_dump(int kind);
int xip_heap_info(uint8_t *out, size_t cap);
int xip_wifi_scan(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap);

/* xip_serial.c: the pins of the serial port, reserved from modules whether or not the port is started. */
uint64_t xip_serial_pins(void);

/* A worker event. Interrupts, alarm timers and other tasks only ever queue these; module code runs on the worker. */
enum { XIP_EV_CALL = 1, XIP_EV_REQUEST, XIP_EV_START, XIP_EV_ALARM, XIP_EV_NETWORK, XIP_EV_WAKE, XIP_EV_PACKET, XIP_EV_TRANSPORT, XIP_EV_IO };
typedef struct {
    uint8_t kind;
    uint32_t inst;                /* instance id (not a pointer: the instance may be gone by the time it is read) */
    int32_t component, method, value;
    uint32_t seq;                 /* ALARM: which arming of the alarm */
    uint64_t id;
    uint8_t alarm;                /* ALARM: index */
} xip_event_t;

/* The argument of an alarm's timer: fixed storage, so a late callback never touches freed memory. */
typedef struct { uint32_t inst, seq; uint8_t index; } xip_alarm_ctx_t;

typedef struct {
    esp_timer_handle_t timer;
    bool used, waiting_net;
    uint32_t seq, window_ms, interval_ms, flags;
    int32_t component, method;
    int64_t due_us;               /* when it was meant to fire (periodic alarms re-arm from this) */
} xip_alarm_t;

struct xip_instance {
    xip_host table;            /* first: xip_host.user points back at the instance */
    char name[16];
    uint32_t id;
    int component;                /* call() address */
    int slot;                     /* index in the registry */
    xip_handle_fn handler;
    xip_done_fn done;
    void *done_ctx;
    uint8_t *state;
    uint32_t state_bytes;
    bool finishing;
    uint8_t *inbox;               /* the one pending request */
    size_t inbox_len;
    int32_t inbox_component, inbox_method;
    bool inbox_full;
    uint8_t *start_request;
    size_t start_len;
    SemaphoreHandle_t waiter_sem; /* a task waits in xip_instance_request: the buffers below are its own */
    uint8_t *waiter_buf; size_t waiter_cap; size_t *waiter_len; int *waiter_rc;
    uint8_t *response;            /* CONFIG_XIP_RESPONSE_MAX */
    size_t response_len;
    bool in_request, responded;
    xip_irq_slot_t irq[XIP_IRQ_SLOTS];
    xip_spi_dev_t spi[XIP_SPI_DEVICES];
    xip_alarm_t alarms[XIP_ALARMS];
    xip_stats_t stats;
    xip_mem mem;               /* the managed heap of this module */
    xip_held held;             /* packets this module owns, and the one the current call lent it */
    struct { int16_t slot; uint16_t start, len; int32_t component, method; uint32_t from; uint64_t id; } pkt_in[CONFIG_XIP_PKT_INBOX];
    uint8_t pkt_head, pkt_count;  /* the ring of packets waiting for this module (under the registry lock) */
};

/* xip_runtime.c */
bool xip_post_isr(const xip_event_t *ev, BaseType_t *higher);
bool xip_connected(void);

/* xip_periph.c: pins, interrupts, buses, analog, LED. Every call checks the pin owner. */
void xip_periph_init(void);
void xip_periph_release(struct xip_instance *inst);
void xip_periph_fill(xip_host *table);

/* Shared status store: owner zero is a native host component. */
int xip_info_init(void);
int xip_rt_info_publish(uint32_t owner, int32_t component, int32_t method, const uint8_t *data, size_t len);
void xip_info_release(uint32_t owner);
int xip_rt_device_info(const xip_host_hooks_t *hooks, uint8_t *out, size_t cap);
