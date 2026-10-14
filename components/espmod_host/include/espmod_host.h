/* SPDX-License-Identifier: Apache-2.0 */
/* The ESP-IDF side of the module ABI (include/espmod.h): builds the host table a module calls.
 *
 * One instance per running module. All module code runs on one worker task, one handler call at a time
 * (abi/API.md, Model): the host sleeps until an alarm, an interrupt or a request. An instance owns everything the
 * module acquires (pins, interrupts, SPI devices, alarms, scratch memory) and gives it all back when it ends
 * (STOP, a handler returning ESPMOD_RET_DONE or an error), so nothing is left behind. The instance does not depend
 * on the loader: an application can create one around a handler it links (how the same module source runs linked
 * or as a flashed image). */
#pragma once

#include <stdbool.h>
#include "esp_err.h"
#include "espmod.h"
#include "espmod_pool.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Where a module's output goes. All hooks may be NULL (events dropped, services not found). */
/* One access point of a scan. `auth` is the driver's authentication mode number. */
typedef struct { uint8_t ssid[32]; uint8_t ssid_len; uint8_t bssid[6]; int8_t rssi; uint8_t channel; uint8_t auth; } espmod_ap_t;

typedef struct {
    /* emit(): called on the worker task. */
    int (*emit)(void *ctx, const char *module, uint16_t event_id, uint8_t value_type,
                const uint8_t *value, size_t len);
    void *emit_ctx;
    /* call_service(): called on the worker task; returns a result for the module. */
    int (*call_service)(void *ctx, const char *module, uint16_t tag, const uint8_t *request,
                        size_t request_len, uint8_t *response, size_t response_capacity,
                        size_t *response_len, uint32_t timeout_ms);
    void *service_ctx;
    /* wifi_* and the module calls of abi/API.md. NULL: not offered; the optional
     * espmod_wifi component implements simple STA hooks in C, and the Rust host has its own STA hooks.
     * espmod_loader implements the module ones; all are called on the
     * worker and must return at once. wifi_status fills state (0 off, 1 joining, 2 up), the name, the IPv4 address
     * and the signal; the host turns it into the CBOR of wifi_info. modules_info writes CBOR (slot records). */
    int (*wifi_start)(const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len);
    void (*wifi_stop)(void);
    int (*wifi_status)(int *state, char ssid[33], char ip[16], int *rssi);
    /* Nonblocking scan: first call starts it and returns -5; later calls return -5 until done, then fill up to `max`
     * entries and return the count. The host turns them into the CBOR array of wifi_scan. */
    int (*wifi_scan)(espmod_ap_t *aps, size_t max);
    /* The device's own (optional) network, kept in NVS by the application: set (empty ssid clears) and read back (1: configured). */
    int (*wifi_config)(const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len);
    int (*wifi_saved)(char ssid[33], int *has_psk);
    /* A named device setting (abi/API.md, nv): value NULL reads into out and returns the length; -1 bad argument, -3 storage,
     * -4 unknown or unset, -5 buffer too small. The application answers for its own names. */
    int (*nv)(const char *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t cap);
    /* The application's own packet pool (DMesh: the QUIC pool), used for every packet a module takes or is sent, instead of one of
     * this host's. NULL: the host's dynamic pool (CONFIG_ESPMOD_PKT_*). Must outlive the host; slots at least ESPMOD_PKT_SLOT_MIN. */
    const espmod_pool_ops *pool;
    int (*modules_info)(uint8_t *out, size_t capacity);
    int (*module_start)(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
    int (*module_stop)(const uint8_t *name, size_t name_len);
    int (*module_manifest)(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);
    /* A module sent a packet to ADDR_MAIN (abi/API.md, send): `from` is its component, `data` the message. The packet is
     * lent for the call and goes back to the pool when the hook returns: copy what must stay. */
    int (*packet)(void *ctx, const char *module, uint32_t from, int component, int method, uint64_t id, const uint8_t *data, size_t len);
    void *packet_ctx;
    /* Backpressure: a module's inbox was full and has room again (called on the worker, after the module released the packet
     * it was handling). The application that feeds the module (a stream: espmod_instance_send answered ESPMOD_ERR_BUSY) opens
     * its window or retries. The queue is on the runtime side; the module takes one packet at a time. */
    void (*inbox_ready)(void *ctx, int component);
    /* Bit n set: GPIO n is the application's (console UART, its own buses, ...). A module that asks for it gets
     * ESPMOD_ERR_UNSUPPORTED. The flash pins of the classic ESP32 (6-11) are always reserved. */
    uint64_t reserved_pins;
    /* Sleepy mode: milliseconds until the next wake window opens (0: none known). Without it a FLEXIBLE alarm fires at
     * its delay. Called from the timer task. */
    uint32_t (*next_window_ms)(void *ctx);
    void *window_ctx;
    /* Optional subset owner, invoked on the serialized worker after profile publication. */
    void (*transport_reconcile)(void);
    int (*transport_set)(const uint8_t *request, size_t len, uint32_t *generation);
    int (*transport_get)(espmod_bearer_profile *profile, uint32_t *generation);
} espmod_host_hooks_t;

/* For an application that cannot build the hook table (Rust): events only, plus the network calls when given. */
typedef int (*espmod_host_emit_fn)(const char *module, uint16_t event_id, uint8_t value_type, const uint8_t *value, size_t len);
typedef struct {
    int (*wifi_start)(const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len);
    void (*wifi_stop)(void);
    int (*wifi_status)(int *state, char ssid[33], char ip[16], int *rssi);
    /* Nonblocking scan: first call starts it and returns -5; later calls return -5 until done, then fill up to `max`
     * entries and return the count. The host turns them into the CBOR array of wifi_scan. */
    int (*wifi_scan)(espmod_ap_t *aps, size_t max);
    /* The device's own (optional) network, kept in NVS by the application: set (empty ssid clears) and read back (1: configured). */
    int (*wifi_config)(const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len);
    int (*wifi_saved)(char ssid[33], int *has_psk);
    /* A named device setting (abi/API.md, nv): value NULL reads into out and returns the length; -1 bad argument, -3 storage,
     * -4 unknown or unset, -5 buffer too small. The application answers for its own names. */
    int (*nv)(const char *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t cap);
    /* The application's own packet pool (DMesh: the QUIC pool), used for every packet a module takes or is sent, instead of one of
     * this host's. NULL: the host's dynamic pool (CONFIG_ESPMOD_PKT_*). Must outlive the host; slots at least ESPMOD_PKT_SLOT_MIN. */
    const espmod_pool_ops *pool;
    int (*modules_info)(uint8_t *out, size_t capacity);
    int (*module_start)(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
    int (*module_stop)(const uint8_t *name, size_t name_len);
    int (*module_manifest)(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);
    int (*packet)(void *ctx, const char *module, uint32_t from, int component, int method, uint64_t id, const uint8_t *data, size_t len);
    void (*inbox_ready)(void *ctx, int component);
    /* Optional subset owner, invoked on the serialized worker after profile publication. */
    void (*transport_reconcile)(void);
    int (*transport_set)(const uint8_t *request, size_t len, uint32_t *generation);
    int (*transport_get)(espmod_bearer_profile *profile, uint32_t *generation);
} espmod_host_ext_t;
esp_err_t espmod_host_init_simple(espmod_host_emit_fn emit, const espmod_host_ext_t *ext);

/* Once, before the first instance. NVS must be initialised by the application (settings). Starts the worker. */
esp_err_t espmod_host_init(const espmod_host_hooks_t *hooks);

/* Connectivity changed (any task): every module gets HOST_NETWORK, and CONNECTED alarms that were waiting are armed. */
void espmod_host_set_connected(bool connected);
/* A wake window opened (any task): modules with a WINDOW alarm get HOST_WAKE. */
void espmod_host_wake_window(void);
/* Desired profile committed: queued on the serialized module worker. */
void espmod_host_transport_changed(uint32_t generation);
/* Desired state API for C owners. Set/get copy a coherent snapshot. */
int espmod_transport_set(const uint8_t *request, size_t len, uint32_t *generation);
int espmod_transport_get(espmod_bearer_profile *profile, uint32_t *generation);

/* Structured status for native host components, using the same cache as modules.
 * Native publications live until replaced or cleared (len zero). Copies the map. */
int espmod_device_info(uint8_t *out, size_t capacity);
int espmod_info_publish(int32_t component, int32_t method, const uint8_t *data, size_t len);

typedef struct espmod_instance espmod_instance_t;

typedef struct {
    uint32_t spi_transfers, spi_errors, irq_events, irq_dropped, emitted, logged;
    uint32_t pkt_sent, pkt_received, pkt_dropped, pkt_leaked, mem_leaked;  /* mem_leaked: bytes still taken when it ended */   /* packets: sent by the module; delivered to it; refused or lost; still held when it ended */
    uint32_t calls, alarms_fired, overruns;   /* handler calls; alarms delivered; calls over CONFIG_ESPMOD_HANDLER_MS */
    uint32_t max_handler_us;
} espmod_stats_t;

/* Called on the worker after the instance has ended and been released (it no longer exists): `result` is what the
 * last handler call returned (ESPMOD_RET_DONE or an error), 0 after a STOP, or ESPMOD_ERR_* for a host failure. */
typedef void (*espmod_done_fn)(void *ctx, int result, const espmod_stats_t *stats);

typedef struct {
    espmod_handle_fn handler;
    uint32_t state_bytes;          /* zeroed state block passed to every call (<= 4096) */
    espmod_done_fn done;
    void *done_ctx;
    int component;                 /* the address of the module for the ABI's call(): its slot tag; 0 = not addressable */
} espmod_instance_config_t;

/* `name` (copied, <= 15 bytes) is used in logs and as the settings namespace "em_<name>". The instance is idle until
 * espmod_instance_start(). */
espmod_instance_t *espmod_instance_create(const char *name, const espmod_instance_config_t *config);
const espmod_host *espmod_instance_host(const espmod_instance_t *inst);

/* Queue HOST_START with `request` (copied). Any task. */
int espmod_instance_start(espmod_instance_t *inst, const uint8_t *request, size_t len);
/* Queue HOST_STOP: the module's handler runs once more, then the instance is released and `done` is called. */
int espmod_instance_stop(espmod_instance_t *inst);
/* Queue a request (copied) for (component, method); one is held per instance: ESPMOD_ERR_BUSY while the previous has
 * not been handled, ESPMOD_ERR_SIZE when larger than CONFIG_ESPMOD_REQUEST_MAX. Any task. */
int espmod_instance_post(espmod_instance_t *inst, int component, int method, const uint8_t *data, size_t len);
/* Same, and wait for the module's respond() (at most CONFIG_ESPMOD_RESPONSE_MAX bytes; none = length 0). */
/* Send the module a message the way a module would: copied into a pool packet and delivered as a call with call.pkt,
 * call.from = ADDR_MAIN. ESPMOD_ERR_BUSY when its inbox is full or the pool is out of packets. Any task. */
int espmod_instance_send(espmod_instance_t *inst, int component, int method, uint64_t id, const uint8_t *data, size_t len);
int espmod_instance_request(espmod_instance_t *inst, int component, int method, const uint8_t *data, size_t len,
                            uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms);

/* The serial port of the board (the console), as hardware the host owns: start and stop it, receive lines, write text.
 * ESP32-C6: USB-Serial/JTAG when a computer is attached at boot, else UART0; ESP32 and S3: UART0 (115200 8N1).
 * Its pins are always reserved from modules. `on_line` runs on the reader task (CONFIG_ESPMOD_SERIAL_STACK) with one
 * edited line (no newline, empty lines dropped), one at a time; it may take its time, but must not call
 * espmod_serial_stop(). */
typedef void (*espmod_serial_line_fn)(void *ctx, char *line, size_t len);
int espmod_serial_start(espmod_serial_line_fn on_line, void *ctx);
/* Stop the reader and release the driver (the port goes back to ESP-IDF's console). Not from `on_line`. */
void espmod_serial_stop(void);
/* Write text; returns the bytes taken, or a negative ESPMOD_ERR_*. */
int espmod_serial_write(const void *data, size_t len);
/* For whoever owns light sleep (the sleepy runtime): a computer is attached on USB-Serial/JTAG (which also charges the device,
 * so a sleep policy can stay awake), and the receive interrupt/wakeup to re-arm after a light-sleep return (ESP-IDF can
 * leave it disabled after a GPIO wake). The reader itself wakes every 250 ms on USB and sleeps on the event queue on UART0. */
bool espmod_serial_usb_active(void);

/* The ESPMOD_FEAT_* bits a module sees in the host table (what the hooks installed at init make real). The loader checks an
 * image's required features against this, so a module that needs SERIAL or PACKETS is refused at install, not at start. */
uint32_t espmod_host_features(void);

/* Send ESP-IDF log lines to the ROM console. For an application that has no C library stdout: Rust std on ESP-IDF 6.0 with its
 * own `app_main` (crates/espmod-host/example) never registers the console, so every ESP_LOG line is lost. Cheap, no allocation;
 * the one place text is formatted outside mod_console, because a log line is the only way those components report. */
void espmod_log_to_rom(void);
void espmod_serial_rearm(void);

/* High-water mark of the worker task's stack, bytes. */
uint32_t espmod_host_stack_high_water(void);
void espmod_instance_stats(const espmod_instance_t *inst, espmod_stats_t *out);

#ifdef __cplusplus
}
#endif
