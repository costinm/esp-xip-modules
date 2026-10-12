/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "esp_timer.h"
#include "espmod_host.h"

#define EM_IRQ_SLOTS 8
#define EM_SPI_DEVICES 4
#define EM_ALARMS 8
#define EM_MAX_INSTANCES 8

struct espmod_instance;
typedef struct {
    struct espmod_instance *inst;
    int pin;
    int32_t component, method;
    volatile bool used;
} em_irq_slot_t;

typedef struct {
    void *handle;                 /* spi_device_handle_t */
    int host;                     /* spi_host_device_t */
    int ready_pin, ready_level;
    uint32_t ready_timeout_ms;
    uint8_t *tx, *rx;             /* DMA-capable bounce buffers */
    void *trans;                  /* spi_transaction_t: lives as long as the driver may still hold it */
    bool pending;                 /* queued, result not yet reclaimed (a timed-out transfer) */
    bool used;
} em_spi_dev_t;

void em_periph_reserve(uint64_t mask);
/* espmod_events.c: the last events modules emitted, and the CBOR of events_info and wifi_info */
void em_events_add(const char *module, uint16_t id, uint8_t type, const uint8_t *value, size_t len);
int em_events_info(uint8_t *out, size_t cap);
int em_wifi_info(const espmod_host_hooks_t *hooks, uint8_t *out, size_t cap);

/* espmod_serial.c: the pins of the serial port, reserved from modules whether or not the port is started. */
uint64_t em_serial_pins(void);

/* A worker event. Interrupts, alarm timers and other tasks only ever queue these; module code runs on the worker. */
enum { EM_EV_CALL = 1, EM_EV_REQUEST, EM_EV_START, EM_EV_ALARM, EM_EV_NETWORK, EM_EV_WAKE };
typedef struct {
    uint8_t kind;
    uint32_t inst;                /* instance id (not a pointer: the instance may be gone by the time it is read) */
    int32_t component, method, value;
    uint32_t seq;                 /* ALARM: which arming of the alarm */
    uint8_t alarm;                /* ALARM: index */
} em_event_t;

/* The argument of an alarm's timer: fixed storage, so a late callback never touches freed memory. */
typedef struct { uint32_t inst, seq; uint8_t index; } em_alarm_ctx_t;

typedef struct {
    esp_timer_handle_t timer;
    bool used, waiting_net;
    uint32_t seq, window_ms, interval_ms, flags;
    int32_t component, method;
    int64_t due_us;               /* when it was meant to fire (periodic alarms re-arm from this) */
} em_alarm_t;

struct espmod_instance {
    espmod_host table;            /* first: espmod_host.user points back at the instance */
    char name[16];
    uint32_t id;
    int component;                /* call() address */
    int slot;                     /* index in the registry */
    espmod_handle_fn handler;
    espmod_done_fn done;
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
    SemaphoreHandle_t waiter_sem; /* a task waits in espmod_instance_request: the buffers below are its own */
    uint8_t *waiter_buf; size_t waiter_cap; size_t *waiter_len; int *waiter_rc;
    uint8_t *response;            /* CONFIG_ESPMOD_RESPONSE_MAX */
    size_t response_len;
    bool in_request, responded;
    uint8_t *arena;
    size_t arena_used;
    em_irq_slot_t irq[EM_IRQ_SLOTS];
    em_spi_dev_t spi[EM_SPI_DEVICES];
    em_alarm_t alarms[EM_ALARMS];
    espmod_stats_t stats;
};

/* espmod_host.c */
bool em_post_isr(const em_event_t *ev, BaseType_t *higher);
bool em_connected(void);

/* espmod_periph.c: pins, interrupts, buses, analog, LED. Every call checks the pin owner. */
void em_periph_init(void);
void em_periph_release(struct espmod_instance *inst);
void em_periph_fill(espmod_host *table);
