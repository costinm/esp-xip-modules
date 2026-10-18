/* SPDX-License-Identifier: Apache-2.0 */
/* The ESP-IDF side of the module ABI (include/xip.h): builds the host table a module calls.
 *
 * One instance per running module. All module code runs on one worker task, one handler call at a time
 * (xip-abi crate docs, Model): the host sleeps until an alarm, an interrupt or a request. An instance owns everything the
 * module acquires (pins, interrupts, SPI devices, alarms, scratch memory) and gives it all back when it ends
 * (STOP, a handler returning XIP_RET_DONE or an error), so nothing is left behind. The instance does not depend
 * on the loader: an application can create one around a handler it links (how the same module source runs linked
 * or as a flashed image). */
#pragma once

#include <stdbool.h>
#include "esp_err.h"
#include "xip.h"
#include "xip_pool.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Where a module's output goes. All hooks may be NULL (events dropped, services not found). */
/* One access point of a scan. `auth` is the driver's authentication mode number. */
typedef struct { uint8_t ssid[32]; uint8_t ssid_len; uint8_t bssid[6]; int8_t rssi; uint8_t channel; uint8_t auth; } xip_ap_t;

typedef struct {
    /* the wifi component and the module calls of include/xip.h. NULL: not offered; the optional
     * xip_wifi component implements simple STA hooks in C, and the Rust host has its own STA hooks.
     * xip_loader implements the module ones; all are called on the
     * worker and must return at once. wifi_state fills xip_wifi_state; the host turns it
     * into the CBOR of wifi_info. modules_info writes CBOR (slot records). */
    int (*wifi_set)(const xip_wifi_intent *intent, uint32_t *generation);
    int (*wifi_get)(xip_wifi_intent *intent, uint32_t *generation);
    int (*wifi_state)(xip_wifi_state *state);
    /* Nonblocking scan: first call starts it and returns -5; later calls return -5 until done, then fill up to `max`
     * entries and return the count. The host turns them into the CBOR array of wifi_scan. */
    int (*wifi_scan)(xip_ap_t *aps, size_t max);
    /* A named device setting (include/xip.h, nv): value NULL reads into out and returns the length; -1 bad argument, -3 storage,
     * -4 unknown or unset, -5 buffer too small. The application answers for its own names. */
    int (*nv)(const char *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t cap);
    /* The application's own packet pool (DMesh: the QUIC pool), used for every packet a module takes or is sent, instead of one of
     * this host's. NULL: the host's dynamic pool (CONFIG_XIP_PKT_*). Must outlive the host; slots at least XIP_PKT_SLOT_MIN. */
    const xip_pool_ops *pool;
    int (*modules_info)(uint8_t *out, size_t capacity);
    int (*module_start)(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
    int (*module_stop)(const uint8_t *name, size_t name_len);
    int (*module_manifest)(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);
    /* A module sent a packet to ADDR_MAIN (include/xip.h, send): `from` is its component, `data` the message. The packet is
     * lent for the call and goes back to the pool when the hook returns: copy what must stay. */
    int (*packet)(void *ctx, const char *module, uint32_t from, int component, int method, uint64_t id, const uint8_t *data, size_t len);
    void *packet_ctx;
    /* Backpressure: a module's inbox was full and has room again (called on the worker, after the module released the packet
     * it was handling). The application that feeds the module (a stream: xip_instance_send answered XIP_ERR_BUSY) opens
     * its window or retries. The queue is on the runtime side; the module takes one packet at a time. */
    void (*inbox_ready)(void *ctx, int component);
    /* Bit n set: GPIO n is the application's (console UART, its own buses, ...). A module that asks for it gets
     * XIP_ERR_UNSUPPORTED. The flash pins of the classic ESP32 (6-11) are always reserved. */
    uint64_t reserved_pins;
    /* Sleepy mode: milliseconds until the next wake window opens (0: none known). Without it a FLEXIBLE alarm fires at
     * its delay. Called from the timer task. */
    uint32_t (*next_window_ms)(void *ctx);
    void *window_ctx;
    /* Optional subset owner, invoked on the serialized worker after profile publication. */
    void (*wifi_reconcile)(void);
    /* The wifi-tuning component (advanced driver settings). A change is announced with xip_host_wifi_changed. */
    int (*wifi_tuning_set)(const xip_wifi_tuning *tuning);
    int (*wifi_tuning_get)(xip_wifi_tuning *tuning);
    /* Promiscuous receive on/off (xip_wifi_monitor, include/xip.h): the frames go to the "promisc" link. NULL: ERR_UNSUPPORTED. */
    int (*wifi_monitor)(const xip_wifi_monitor *monitor);
} xip_host_hooks_t;

/* For an application that cannot build the hook table (Rust): the Wi-Fi, settings and module-control calls when given. */
typedef struct {
    int (*wifi_set)(const xip_wifi_intent *intent, uint32_t *generation);
    int (*wifi_get)(xip_wifi_intent *intent, uint32_t *generation);
    int (*wifi_state)(xip_wifi_state *state);
    /* Nonblocking scan: first call starts it and returns -5; later calls return -5 until done, then fill up to `max`
     * entries and return the count. The host turns them into the CBOR array of wifi_scan. */
    int (*wifi_scan)(xip_ap_t *aps, size_t max);
    /* A named device setting (include/xip.h, nv): value NULL reads into out and returns the length; -1 bad argument, -3 storage,
     * -4 unknown or unset, -5 buffer too small. The application answers for its own names. */
    int (*nv)(const char *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t cap);
    /* The application's own packet pool (DMesh: the QUIC pool), used for every packet a module takes or is sent, instead of one of
     * this host's. NULL: the host's dynamic pool (CONFIG_XIP_PKT_*). Must outlive the host; slots at least XIP_PKT_SLOT_MIN. */
    const xip_pool_ops *pool;
    int (*modules_info)(uint8_t *out, size_t capacity);
    int (*module_start)(const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len);
    int (*module_stop)(const uint8_t *name, size_t name_len);
    int (*module_manifest)(const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity);
    int (*packet)(void *ctx, const char *module, uint32_t from, int component, int method, uint64_t id, const uint8_t *data, size_t len);
    void (*inbox_ready)(void *ctx, int component);
    /* Optional subset owner, invoked on the serialized worker after profile publication. */
    void (*wifi_reconcile)(void);
    int (*wifi_tuning_set)(const xip_wifi_tuning *tuning);
    int (*wifi_tuning_get)(xip_wifi_tuning *tuning);
    int (*wifi_monitor)(const xip_wifi_monitor *monitor);
} xip_host_ext_t;
esp_err_t xip_host_init_simple(const xip_host_ext_t *ext);

/* Once, before the first instance. NVS must be initialised by the application (settings). Starts the worker. */
esp_err_t xip_host_init(const xip_host_hooks_t *hooks);

/* The sleep owner (xip_power.c, the `power` component): the radio steps around a light sleep. `suspend` stops the radio the way an epoch ends and returns
 * whether it was up; `resume` brings it back with the same settings. NULL: sleep without touching the radio (an application that owns the radio). */
typedef struct {
    bool (*suspend)(void);
    void (*resume)(void);
} xip_power_radio;
void xip_power_set_radio(const xip_power_radio *radio);

/* Connectivity changed (any task): every module gets HOST_NETWORK, and CONNECTED alarms that were waiting are armed. */
void xip_host_set_connected(bool connected);
/* A wake window opened (any task): modules with a WINDOW alarm get HOST_WAKE. */
void xip_host_wake_window(void);
/* The same, with the reason as the value of HOST_WAKE (a sleep ended: the wake cause). */
void xip_host_wake_window_value(int32_t value);
/* The wanted Wi-Fi state was committed: queued on the serialized module worker (wifi_reconcile, then HOST_WIFI to modules). */
void xip_host_wifi_changed(uint32_t generation);
/* The runtime's own transports (the `link` component): a driver registers a named link with the function that transmits a
 * frame, and hands every received frame to xip_link_native_rx (any task, including the driver's own: copied into a pool packet,
 * stamped with the time of the call, routed or delivered to the subscribers; dropped when nobody listens). `dest` is the
 * destination token of `send` (0: broadcast), `source` the token of the sender, `signal` as xip_link_signal().
 *
 * Transmit: `tx` gets the frame in a pool packet that the runtime owns until the driver is done. It returns a negative error to
 * refuse (XIP_ERR_BUSY: the sender keeps the packet and waits for the READY event; call xip_link_native_ready when there is room),
 * XIP_LINK_TX_DONE when the frame is already sent, or 0 when it will be: the driver then calls xip_link_native_tx_done(addr,
 * pkt->slot, outcome) exactly once, from any task. `data` may be moved back into `headroom` to prepend the driver's envelope. */
typedef struct { uint8_t *data; size_t len; size_t headroom; int slot; } xip_link_packet;
#define XIP_LINK_TX_DONE 1
typedef int (*xip_link_tx_fn)(void *ctx, uint64_t dest, const xip_link_packet *pkt);
int xip_link_native_register(const char *name, uint32_t mtu, uint32_t flags, xip_link_tx_fn tx, void *ctx);
void xip_link_native_unregister(int addr);
int xip_link_native_set_state(int addr, uint32_t state, uint32_t rate_kbps);
void xip_link_native_ready(int addr);
void xip_link_native_tx_done(int addr, int slot, int outcome);
int xip_link_native_rx(int addr, uint64_t source, int32_t signal, const uint8_t *data, size_t len);
/* The same without the copy: begin (0 + a packet to fill, 1 nobody listens, or an error), parse into buf->data, then commit or abort. */
typedef struct { uint8_t *data; size_t capacity; int slot; } xip_link_rx_buf;
int xip_link_native_rx_begin(int addr, xip_link_rx_buf *buf);
void xip_link_native_rx_commit(int addr, const xip_link_rx_buf *buf, size_t len, uint64_t source, int32_t signal);
void xip_link_native_rx_abort(const xip_link_rx_buf *buf);

/* The wanted state for C owners (the same as the wifi component). Copies a coherent snapshot. */
int xip_wifi_set_intent(const xip_wifi_intent *intent, uint32_t *generation);
int xip_wifi_get_intent(xip_wifi_intent *intent, uint32_t *generation);
int xip_wifi_get_tuning(xip_wifi_tuning *tuning);

/* Structured status for native host components, using the same cache as modules.
 * Native publications live until replaced or cleared (len zero). Copies the map. */
int xip_device_info(uint8_t *out, size_t capacity);
int xip_info_publish(int32_t component, int32_t method, const uint8_t *data, size_t len);

typedef struct xip_instance xip_instance_t;

typedef struct {
    uint32_t spi_transfers, spi_errors, irq_events, irq_dropped, emitted, logged;
    uint32_t pkt_sent, pkt_received, pkt_dropped, pkt_leaked, mem_leaked;  /* mem_leaked: bytes still taken when it ended */   /* packets: sent by the module; delivered to it; refused or lost; still held when it ended */
    uint32_t calls, alarms_fired, overruns;   /* handler calls; alarms delivered; calls over CONFIG_XIP_HANDLER_MS */
    uint32_t max_handler_us;
} xip_stats_t;

/* Called on the worker after the instance has ended and been released (it no longer exists): `result` is what the
 * last handler call returned (XIP_RET_DONE or an error), 0 after a STOP, or XIP_ERR_* for a host failure. */
typedef void (*xip_done_fn)(void *ctx, int result, const xip_stats_t *stats);

typedef struct {
    xip_handle_fn handler;
    uint32_t state_bytes;          /* zeroed state block passed to every call (<= 4096) */
    xip_done_fn done;
    void *done_ctx;
    int component;                 /* the address of the module for the ABI's call(): its tag; 0 = not addressable */
    uint32_t port;                 /* manifest key 9: the datagram port whose packets it receives and answers (XIP_PORT_CONSOLE: the serial line); 0 = none */
    uint32_t stream;               /* manifest key 10: the stream port: TCP port XIP_PORT_NET + stream, connections arrive as socket events; 0 = none */
} xip_instance_config_t;

/* `name` (copied, <= 15 bytes) is used in logs and as the settings namespace "em_<name>". The instance is idle until
 * xip_instance_start(). */
xip_instance_t *xip_instance_create(const char *name, const xip_instance_config_t *config);
const xip_host *xip_instance_host(const xip_instance_t *inst);

/* Queue HOST_START with `request` (copied). Any task. */
int xip_instance_start(xip_instance_t *inst, const uint8_t *request, size_t len);
/* Queue HOST_STOP: the module's handler runs once more, then the instance is released and `done` is called. */
int xip_instance_stop(xip_instance_t *inst);
/* Queue a request (copied) for (component, method); one is held per instance: XIP_ERR_BUSY while the previous has
 * not been handled, XIP_ERR_SIZE when larger than CONFIG_XIP_REQUEST_MAX. Any task. */
int xip_instance_post(xip_instance_t *inst, int component, int method, const uint8_t *data, size_t len);
/* Same, and wait for the module's answer (a message to the application with the request id; at most CONFIG_XIP_RESPONSE_MAX bytes; none = length 0). */
/* Send the module a message the way a module would: copied into a pool packet and delivered as a call with call.pkt,
 * call.from = ADDR_MAIN. XIP_ERR_BUSY when its inbox is full or the pool is out of packets. Any task. */
int xip_instance_send(xip_instance_t *inst, int component, int method, uint64_t id, const uint8_t *data, size_t len);
int xip_instance_request(xip_instance_t *inst, int component, int method, const uint8_t *data, size_t len,
                            uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms);

/* The serial port of the board (the console), as hardware the host owns: start and stop it, receive lines, write text.
 * ESP32-C6: USB-Serial/JTAG when a computer is attached at boot, else UART0; ESP32 and S3: UART0 (115200 8N1).
 * Its pins are always reserved from modules. `on_line` runs on the reader task (CONFIG_XIP_SERIAL_STACK) with one
 * edited line (no newline, empty lines dropped), one at a time; it may take its time, but must not call
 * xip_serial_stop(). */
typedef void (*xip_serial_line_fn)(void *ctx, char *line, size_t len);
int xip_serial_start(xip_serial_line_fn on_line, void *ctx);
/* Stop the reader and release the driver (the port goes back to ESP-IDF's console). Not from `on_line`. */
void xip_serial_stop(void);
/* Write text; returns the bytes taken, or a negative XIP_ERR_*. */
int xip_serial_write(const void *data, size_t len);

/* The serial line carries ports (include/xip.h, XIP_PORT_*). By default it is what the console uses: text lines are frames for
 * XIP_PORT_CONSOLE and a frame sent to a port is written as text. An application can install a codec that does more (the Rust runtime's
 * PPP framing: records that carry their port in the first byte, text lines still for the console). `on_bytes` gets every chunk the driver
 * reads (reader task) and cuts frames with xip_serial_rx_begin / commit; `on_reset` drops a partial frame when the input was flushed;
 * `tx` writes one frame for a port. */
typedef struct {
    void (*on_bytes)(const uint8_t *data, size_t len);
    void (*on_reset)(void);
    int (*tx)(uint32_t port, const uint8_t *data, size_t len);
} xip_serial_codec_t;
void xip_serial_set_codec(const xip_serial_codec_t *codec);
/* Write one frame for `port` with the codec, or as plain text without one. */
int xip_serial_write_frame(uint32_t port, const void *data, size_t len);
/* Receive a frame for `port` without a copy: begin returns 0 and a pool packet to fill (capacity XIP_PKT_MAX), 1 when no module has the port
 * (skip the frame), or an error (XIP_ERR_NOMEM: the pool is empty). Then commit (the frame is `len` bytes) or abort. The token of the frame is its port. */
int xip_serial_rx_begin(uint32_t port, xip_link_rx_buf *buf);
void xip_serial_rx_commit(uint32_t port, const xip_link_rx_buf *buf, size_t len);
void xip_serial_rx_abort(const xip_link_rx_buf *buf);
/* The same for any link: `link` is its address and `token` the id of the far end (a connection, a port). */
int xip_port_rx_begin(uint32_t port, xip_link_rx_buf *buf);
void xip_port_rx_commit(int link, uint32_t port, const xip_link_rx_buf *buf, size_t len, uint64_t token);
void xip_port_rx_abort(const xip_link_rx_buf *buf);

/* For whoever owns light sleep (the sleepy runtime): a computer is attached on USB-Serial/JTAG (which also charges the device,
 * so a sleep policy can stay awake), and the receive interrupt/wakeup to re-arm after a light-sleep return (ESP-IDF can
 * leave it disabled after a GPIO wake). The reader itself wakes every 250 ms on USB and sleeps on the event queue on UART0. */
bool xip_serial_usb_active(void);


/* Send ESP-IDF log lines to the ROM console. For an application that has no C library stdout: Rust std on ESP-IDF 6.0 with its
 * own `app_main` (main-rs) never registers the console, so every ESP_LOG line is lost. Cheap, no allocation;
 * the one place text is formatted outside mod_con, because a log line is the only way those components report. */
void xip_log_to_rom(void);
void xip_serial_rearm(void);

/* High-water mark of the worker task's stack, bytes. */
uint32_t xip_host_stack_high_water(void);
void xip_instance_stats(const xip_instance_t *inst, xip_stats_t *out);

#ifdef __cplusplus
}
#endif
