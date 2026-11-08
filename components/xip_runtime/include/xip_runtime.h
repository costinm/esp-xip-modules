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
#include "xip_quic.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Where a module's output goes. All hooks may be NULL (events dropped, services not found). */
/* One access point of a scan. `auth` is the driver's authentication mode number. */
typedef struct { uint8_t ssid[32]; uint8_t ssid_len; uint8_t bssid[6]; int8_t rssi; uint8_t channel; uint8_t auth; } xip_ap_t;

typedef struct {
    /* the wifi component and the module calls of include/xip.h. NULL: not offered; the optional
     * xip_wifi component implements the shared generic C/Rust radio owner.
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
    /* Look at the module partition again (include/xip.h, module_rescan): the number of modules, or negative. NULL: ERR_UNSUPPORTED. */
    int (*module_rescan)(void);
    /* Start the installed module that hosts the component `name` (manifest key 11) if it is not running; 0 when it is (the instance exists, its HOST_START
     * is run by get_component itself). get_component calls it for a name it cannot serve. NULL: no starting on demand. */
    int (*component_start)(const char *name);
    /* A module sent a packet to ADDR_MAIN (include/xip.h, send): `from` is its component, `data` the message. The packet is
     * lent for the call and goes back to the pool when the hook returns: copy what must stay. */
    int (*packet)(void *ctx, const char *module, uint32_t from, int component, int method, uint64_t id, const uint8_t *data, size_t len);
    void *packet_ctx;
    /* Backpressure hint: the global packet queue has descriptor room again.
     * Called on the worker for live components; pool pressure may still refuse a retry. */
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
    /* Resolve an installed wire component or datagram port; worker only. */
    int (*request_start)(int32_t component, uint32_t port);
} xip_host_hooks_t;

/* For an application that cannot build the hook table (Rust): the Wi-Fi, settings and module-control calls when given.
 * Generic firmware first uses xip_wifi_install_simple; products supply their
 * own callbacks and keep radio and sleep ownership with the application. */
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
    int (*module_rescan)(void);   /* appended: see xip_host_hooks_t */
    int (*component_start)(const char *name);   /* appended: see xip_host_hooks_t */
    int (*request_start)(int32_t component, uint32_t port);
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

/* The BLE component's part of a sleep (xip_ble): `suspend` runs before the radio is stopped and returns a negative XIP_ERR_* to veto the sleep (a
 * live BLE link or service, per its mode); `resume` runs after the radio is back, also after a rejected sleep. NULL clears. The table must outlive the runtime. */
typedef struct {
    int (*suspend)(void);
    int (*resume)(void);
} xip_power_ble;
void xip_power_set_ble(const xip_power_ble *ble);

/* A component of another ESP-IDF component (xip_ble) offers its table to get_component(name). The name and table must stay valid. */
int xip_component_register(const char *name, const void *table);

/* Connectivity changed (any task): every module gets HOST_NETWORK, and CONNECTED alarms that were waiting are armed. */
void xip_host_set_connected(bool connected);
/* Serialized radio owner: release IP listeners/connections before teardown. */
void xip_host_network_suspend(void);
uint8_t xip_host_network_sockets(void);
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
/* tx() receives exclusive mutable access to one pool lease. It must not retain,
 * clone, or release that lease: synchronous DONE returns it to the runtime;
 * asynchronous acceptance holds it until tx_done exactly once. BUSY leaves the
 * original with the sender. The runtime blocks TX while read borrows remain. */
typedef struct { uint8_t *data; size_t len; size_t headroom; int slot; xip_pool *pool; } xip_link_packet;
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
/* NOW callbacks use prepared storage only; prepare on the serialized owner. */
void xip_link_native_rx_prepare(void);
int xip_link_native_rx_begin_cached(int addr, xip_link_rx_buf *buf);
void xip_link_native_rx_commit(int addr, const xip_link_rx_buf *buf, size_t len, uint64_t source, int32_t signal);
void xip_link_native_rx_abort(const xip_link_rx_buf *buf);

/* The common QUIC receive function (in process, one function for every link). Frames that are QUIC datagrams reach `rx` in a pool packet with the
 * link described in `info`: a frame of a link that carries no port (ESP-NOW, the BLE CoC channel, the UDP link on the default port 3339), and a
 * PPP record of the port XIP_PORT_QUIC (0xf7). Ownership, not fan-out: `rx` returns whether it TOOK the frame. If it did, the frame is its
 * own (it copied what it keeps; the runtime returns the packet to the pool) and nobody else sees it; if it declined, normal routing goes on
 * (routes, then the subscribers of the link or the module bound to the port). The payload is in the pool packet and the headroom in front of it
 * stays empty, so a frame can be forwarded as IPv6/UDP without a copy. Called on the receiving task (a driver's task): must not block. Other
 * UDP ports, TCP, the NAN port and the other PPP ports keep their dispatch. Events (`ready`, `state`) are broadcast. Modules do not use this
 * function: they subscribe to ports or to a component and message. NULL clears. */
typedef struct {
    int link;                  /* the link's address */
    const char *name;          /* "espnow", "ble", ...; valid for the call */
    uint64_t source;           /* the far end's token */
    int32_t signal;            /* RSSI or 0 */
    uint32_t time_us;          /* radio receive time */
    uint32_t port;             /* 0: the link has no port; XIP_PORT_QUIC for a PPP record of that port */
} xip_rx_info;
typedef struct {
    void *ctx;
    bool (*rx)(void *ctx, const xip_rx_info *info, const uint8_t *data, size_t len);
    void (*ready)(void *ctx, int link);
    void (*state)(void *ctx, int link, uint32_t state);
} xip_quic_rx_ops;
void xip_quic_rx_set(const xip_quic_rx_ops *ops);

/* ---- The classifier behind the common QUIC receive function (xip_quic.h has the formats) ----
 * Enabled by the application (main.c does): after
 * `xip_quic_classifier_enable(true)` every frame of a QUIC link is classified by its first byte BEFORE `xip_quic_rx_set`'s function is asked. QUIC links: the PPP record of port XIP_PORT_QUIC, the links `ble`, `udp` and `espnow` (the NAN carrier is told by its NSY1 magic and goes to the NAN port before the link), and the names added with
 * `xip_quic_link_enable`. Exactly one handler gets a packet and
 * is responsible for it:
 *  - fixed bit clear: not QUIC, DECLINED: the old path goes on (`xip_quic_rx_set`'s function, routes, subscribers);
 *  - truncated or invalid header, or more than XIP_QUIC_MAX_PACKET bytes: dropped and counted;
 *  - SHORT header: the DCID (length from the quic-lite class rule, or the fixed `xip_quic_short_cid_len`) is looked up in the relay table. A LOCAL
 *    entry hands the packet to the `local` callback (ownership passes: it must release it with xip_quic_pkt_release or pass it on with
 *    xip_quic_pkt_send). A RELAY entry forwards the same pool packet to its link and peer address with the DCID replaced (forward or drop and count: no queue, no retransmission). Unknown DCID: offered to the attached module endpoint, otherwise dropped and counted;
 *  - LONG header, version XIP_QUIC_VERSION_CBOR (0x444d0002): after the SCID comes the CBOR envelope {1: component, 2: method, 3: id, 5: fields}. It
 *    goes to the module claiming its component in manifest key 13 (or hosting it as its own tag), or to the module of that component if it has a datagram port or hosts a table; otherwise to the
 *    console module (component 20), which serves console and native methods. This is the only place an envelope is parsed: the runtime parses it
 *    (xip_envelope.h) and the handler gets an ordinary request, XIP_CALL_ENVELOPE; module ports deliver raw frames. The module's answer to the link and token it came from is sent as a 0x444d0002 long packet (DCIDs
 *    swapped), within XIP_QUIC_REPLY_MS;
 *  - LONG header, any other version (0x444d0001 of quic-lite, versions that are not ours): given to the same `local` callback, else dropped and counted. */
typedef struct { int slot; uint16_t start, len; } xip_quic_pkt;           /* a pool packet that the callback owns */
const uint8_t *xip_quic_pkt_data(const xip_quic_pkt *p, size_t *len);      /* the whole QUIC packet (headroom in front stays empty) */
void xip_quic_pkt_release(xip_quic_pkt *p);                                /* return it to the pool; any task; p->slot = -1 */
/* Send the packet on a link as it is (no copy). Like every send: consumed on success and on every failure except XIP_ERR_BUSY, which leaves it with
 * the caller to retry after the link's `ready` or to release. This primitive does not look up a route; the common receive path performs table-based relay forwarding. */
int xip_quic_pkt_send(xip_quic_pkt *p, int link, uint64_t token);
typedef struct {
    void *ctx;
    /* `hdr` points into the packet. Called on the receiving task (a driver's task): must not block. */
    void (*rx)(void *ctx, const xip_rx_info *info, const xip_quic_hdr *hdr, xip_quic_pkt *pkt);
} xip_quic_local_ops;
void xip_quic_local_set(const xip_quic_local_ops *ops);                    /* NULL clears */
void xip_quic_classifier_enable(bool on);
/* The relay table (runtime-issued IDs; a vector that grows to XIP_QUIC_TABLE_MAX). Callable from any task. */
/* kind: xip_quic_kind. RELAY: `egress_link` (xip_link_find) and `token` (the peer's address on it) say where short packets go, `out_cid` is the DCID the next hop
 * knows the circuit by. LOCAL: the other arguments are unused. */
int xip_quic_table_add_dcid(const uint8_t *cid, size_t len, unsigned kind, int egress_link, uint64_t token, const uint8_t *out_cid, size_t out_len);
/* The runtime issues the IDs: the lowest free quic-lite CID (1 byte up to 127 entries) is added and returned in `cid` (XIP_QUIC_CID_MAX bytes); reuse after remove. */
int xip_quic_table_alloc_dcid(unsigned kind, int egress_link, uint64_t token, const uint8_t *out_cid, size_t out_len, uint8_t *cid, size_t *len);
int xip_quic_table_remove_dcid(const uint8_t *cid, size_t len);
unsigned xip_quic_table_count(void);
void xip_quic_short_cid_len(unsigned len);                                 /* 0 (default): the quic-lite class rule */
int xip_quic_link_enable(const char *name);                                /* one more QUIC link by name (up to 4) */
void xip_quic_stats_get(xip_quic_stats *out);
/* The address of the native link `name`, or XIP_ERR_NOTFOUND; whether it is XIP_LINK_UP; and one frame sent on it (see xip_link_native_send). */
int xip_link_find(const char *name);
bool xip_link_native_up(int addr);
int xip_link_native_send(int addr, uint64_t dest, const uint8_t *data, size_t len);

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
    uint32_t pkt_sent, pkt_received, pkt_dropped, pkt_leaked, mem_leaked;  /* pkt_leaked is reserved (0); packet holders are module-owned. mem_leaked counts outstanding managed bytes. */
    uint32_t calls, alarms_fired, overruns;   /* handler calls; alarms delivered; calls over CONFIG_XIP_HANDLER_MS */
    uint32_t max_handler_us;
} xip_stats_t;

/* Called on the worker after the instance has ended and been released (it no longer exists): `result` is what the
 * last handler call returned (XIP_RET_DONE or an error), 0 after a STOP, or XIP_ERR_* for a host failure. */
typedef void (*xip_done_fn)(void *ctx, int result, const xip_stats_t *stats);

/* Most wire components one module may claim in manifest key 13 (the envelope owners). */
#define XIP_WIRE_MAX 8

/* One wire component claimed by a module (manifest key 13: an array of [id, version] pairs). `id` is a positive int32, `version` a uint32; id 0 = unused. */
typedef struct {
    int32_t id;
    uint32_t version;
} xip_wire_component_t;

typedef struct {
    xip_handle_fn handler;
    uint32_t state_bytes;          /* zeroed state block passed to every call (<= 4096) */
    xip_done_fn done;
    void *done_ctx;
    int component;                 /* the address of the module for the ABI's call(): its tag; 0 = not addressable */
    uint32_t port;                 /* manifest key 9: the datagram port whose packets it receives and answers (XIP_PORT_CONSOLE: the serial line); 0 = none */
    uint32_t stream;               /* manifest key 10: the stream port: TCP port XIP_PORT_NET + stream, connections arrive as socket events; 0 = none */
    char components[XIP_COMPONENTS_MAX][16];   /* manifest key 11: names of the components the module hosts (copied); "" = unused. get_component(name) falls
                                    * through to the running module that hosts it: the caller gets a table of runtime thunks that take the global module
                                    * lock, call the module's function with the module's context, and return XIP_ERR_NOTFOUND once the module is gone. */
    xip_wire_component_t wire[XIP_WIRE_MAX];   /* manifest key 13: additional wire components (envelope owners) with their versions; id 0 = unused. */
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
 * call.from = ADDR_MAIN. XIP_ERR_BUSY when the global delivery queue is full or the pool is out of packets. Any task. */
int xip_instance_send(xip_instance_t *inst, int component, int method, uint64_t id, const uint8_t *data, size_t len);
int xip_instance_request(xip_instance_t *inst, int component, int method, const uint8_t *data, size_t len,
                            uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms);

/* The serial port of the board, the wire the runtime reads: started once at xip_host_init, with
 * the runtime's PPP codec (xip_ppp.c) from the first byte. The UART speaks the runtime's framing
 * exclusively: a record is `0x7e port bytes 0x7e`, escaped; the record's port decides which
 * module receives it (ports of modules: through xip_port_rx_begin / commit; the port byte itself
 * is not part of the frame). The serial text mode is not on the UART: the person's console is the
 * TCP lane (the console module's stream port). A record no module holds is skipped whole. */
int xip_serial_start(void);
/* Product adapters share the same physical driver/tasks, while retaining their
 * own wire codec and pool. open stops/releases the previous reader first.
 * start_client consumes ctx on success AND failure via on_close, after all
 * callbacks are quiescent. TX is independent of RX; callbacks run in tasks. */
typedef struct {
    void *ctx;
    void (*on_bytes)(void *ctx, const uint8_t *data, size_t len);
    void (*on_reset)(void *ctx);
    void (*on_tx)(void *ctx);
    void (*on_uart_event)(void *ctx);
    void (*on_close)(void *ctx);
} xip_serial_client_t;
/* Application-entry and transition heap checkpoints do not allocate. */
void xip_heap_checkpoint(const char *phase);
int xip_serial_open(int baud, size_t usb_buffer_size);
int xip_serial_start_client(const xip_serial_client_t *client);
/* The packet API uses the same C PPP codec as module serial. C consumes ctx on
 * start (also failure). Busy submit never calls take: source keeps its packet.
 * Accepted submit calls take synchronously under admission lock, then owns the
 * packet until complete. One physical slot, no FIFO or payload copy. */
int xip_serial_start_packets(const xip_serial_packet_client *client);
int xip_serial_submit_packet(void *source, void (*take)(void *ctx, void *source, xip_serial_packet_tx *packet));
bool xip_serial_tx_available(void);
void xip_serial_receive_enabled(bool enabled);
int xip_serial_frame_begin(uint32_t port, xip_link_rx_buf *buf);
void xip_serial_frame_finish(uint32_t port, const xip_link_rx_buf *buf, size_t len, int outcome);
void xip_serial_tx_ready(void);
int xip_serial_set_baud(uint32_t baud);
/* Native serial owner: queue a volatile rate change, or handle decoded
 * control ports 0xf8 (wake) / 0xf9 (request+BE32 rate). ACK drains first.
 * USB rejects rate changes. No packet allocation or QUIC dependency. */
int xip_serial_request_baud(uint32_t baud);
int xip_serial_control(uint32_t port, const uint8_t *data, size_t len);
void xip_serial_activity_window(uint32_t ms);
void xip_serial_activate(void);
/* Local RX/wake edge requests an awake ACK; worker emits PPP 0xf8 payload 01
 * after restoring active module roles. No complete wake request is required. */
void xip_serial_note_wake(void);
void xip_serial_ack_awake(void);
/* Generic activity mechanism; policy decides whether/how to sleep.
 * activate extends (never shortens) the current interval; zero is a no-op. */
void xip_power_activate(uint32_t duration_ms);
void xip_power_clear_activity(void);
uint32_t xip_power_active_remaining_ms(void);
int xip_serial_buffered_bytes(size_t *bytes);
/* Stop the reader and release the driver (the port goes back to ESP-IDF's console). */
void xip_serial_stop(void);
/* Write text; returns the bytes taken, or a negative XIP_ERR_*. The runtime's log calls use this outside the codec. */
int xip_serial_write(const void *data, size_t len);
/* Drain the current UART frame before explicit light sleep; never closes RX. */
int xip_serial_drain(void);

/* The framing of the serial line. `on_bytes` gets every chunk the driver reads (reader task) and
 * cuts records with xip_serial_rx_begin / commit; `on_reset` drops a partial record when the
 * input was flushed; `tx` writes one frame for a port. An application can install its own codec
 * (the runtime installs its PPP codec at start; a product may frame differently, e.g. its own
 * records lane). */
typedef struct {
    void (*on_bytes)(const uint8_t *data, size_t len);
    void (*on_reset)(void);
    int (*tx)(uint32_t port, const uint8_t *data, size_t len);
} xip_serial_codec_t;
void xip_serial_set_codec(const xip_serial_codec_t *codec);
/* Write one frame for `port` through the codec (the bytes on the wire, framed). */
int xip_serial_write_frame(uint32_t port, const void *data, size_t len);
/* Receive a frame for `port` without a copy: begin returns 0 and a pool packet to fill
 * (capacity XIP_PKT_MAX), 1 when no module has the port (skip the frame), or an error
 * (XIP_ERR_NOMEM: the pool is empty). Then commit (the frame is `len` bytes) or abort. The token
 * of the frame is its port. */
int xip_serial_rx_begin(uint32_t port, xip_link_rx_buf *buf);
void xip_serial_rx_commit(uint32_t port, const xip_link_rx_buf *buf, size_t len);
void xip_serial_rx_abort(const xip_link_rx_buf *buf);
/* The same for any link: `link` is its address and `token` the id of the far end (a connection, a port). */
int xip_port_rx_begin(uint32_t port, xip_link_rx_buf *buf);
/* Radio RX never grows the pool. Missing cached storage means drop. */
int xip_port_rx_begin_cached(uint32_t port, xip_link_rx_buf *buf);
void xip_port_rx_commit_radio(int link, uint32_t port, const xip_link_rx_buf *buf,
    size_t len, uint64_t token, int32_t signal, uint32_t time_us);
void xip_port_rx_commit(int link, uint32_t port, const xip_link_rx_buf *buf, size_t len, uint64_t token);
/* Source of a datagram received on a runtime-owned UDP port (the call's `src`).
 * IPv4 is represented as IPv4-mapped IPv6. */
typedef struct { uint8_t ip[16]; uint16_t port; uint32_t scope; } xip_udp_address;
/* A datagram of a runtime-owned UDP port: not a link — `link` is the socket handle
 * (`ADDR_UDP + fd`, received on `call.from`) and `src` rides with the deliver as the
 * call's source; the module answers to it with `udp_send` on the handle. */
void xip_port_rx_commit_udp(uint32_t link, uint32_t port, const xip_link_rx_buf *buf, size_t len, const xip_udp_address *src);
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
/* Rust panic quarantine, retained across software restarts until power-on. */
bool xip_module_panicked(const char *name);
void xip_instance_stats(const xip_instance_t *inst, xip_stats_t *out);

#ifdef __cplusplus
}
#endif

/* Resolve a configured UDP circuit destination against the current listener. */
int xip_quic_udp_destination(const uint8_t address[22], uint64_t *token);
