/* SPDX-License-Identifier: Apache-2.0 */
/* The `nimble` component: a thin subset of NimBLE with no product policy (crates/xip-abi/src/nimble.rs). What is advertised (UUIDs, layout, intervals,
 * name, scan response), the PSM, the scan filters and the size and buffer count of the CoC pool arrive as arguments of start/scan/connect from
 * the module that owns them (modules/mod_ble hosts the `ble` component, 104). The text below was written when this was the `ble` component
 * of DMesh Main; the mechanics are unchanged.
 *
 * The former `ble` component. The NimBLE mechanics are fw/ble/native/dmesh_nimble/dmesh_nimble.c as DMesh Main ran them (advertising restart after a
 * GAP disconnect, scan/advertise exclusion, the CoC server and client, the single-record CoC transmit buffer, host-task-only GAP calls): the
 * function bodies are the same, grouped under `Mechanics`. What changed in the move:
 *   - the Rust wrapper's state (fw/ble/src/lib.rs: ready, connection generation, counters, last scan result, bonded) and hooks live here,
 *     in C, behind the `xip_nimble` table (crates/xip-abi/src/nimble.rs) and the `xip_ble_*` functions of xip_ble.h;
 *   - the pieces of the NimBLE configuration this component owns are Kconfig options (Kconfig); roles and CoC follow the stack configuration
 *     (MYNEWT_VAL) so that a build without them links none of that code;
 *   - the wake/sleep modes (suspend/resume, the power vote) and the scan cache are additions; everything else is the old behavior.
 * The CoC channel is the native link "ble": a received SDU is copied once into a pool packet and offered to the common QUIC receive function
 * (xip_quic_rx_set); a sent packet is a pool packet the runtime lends to link_tx, copied once into an mbuf on the host task and returned with
 * xip_link_native_tx_done, for accepted and failed sends alike (READY then tells the sender the channel is free).
 * The security configuration (bonding, secure connections, no MITM, level 2, key distribution) is the one of dmesh_nimble_init and is not an option. */
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>

#include "esp_err.h"
#include "esp_attr.h"
#include "esp_log.h"
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/semphr.h"
#include "xip_runtime.h"
#include "xip_ble.h"

#if CONFIG_XIP_BLE && CONFIG_BT_ENABLED && CONFIG_BT_NIMBLE_ENABLED

#include "esp_bt.h"
#include "esp_heap_caps.h"
#include "host/ble_gap.h"
#include "host/ble_hs.h"
#include "host/ble_hs_id.h"
#include "host/ble_hs_adv.h"
#include "host/ble_l2cap.h"
#include "host/ble_store.h"
#include "host/util/util.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "nimble/nimble_npl.h"
#include "os/os_mbuf.h"
#include "syscfg/syscfg.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

static const char *TAG = "xip_ble";

/* What is compiled in follows the NimBLE configuration of the image. */
#if defined(CONFIG_XIP_BLE_COC) && MYNEWT_VAL(BLE_L2CAP_COC_MAX_NUM) >= 1
#define XB_COC 1
#else
#define XB_COC 0
#endif
#define XB_OBSERVER MYNEWT_VAL(BLE_ROLE_OBSERVER)
#define XB_CENTRAL MYNEWT_VAL(BLE_ROLE_CENTRAL)
#define XB_PERIPHERAL MYNEWT_VAL(BLE_ROLE_PERIPHERAL)

/* Log lines of the mechanics (formerly the DMesh "event type=ble.*" lines): 0 none, 1 failures, 2 all. */
static void log_line(const char *line) { ESP_LOGI(TAG, "%s", line); }
#if CONFIG_XIP_BLE_LOG_LEVEL >= 1
#define XB_LOG(level, ...) do { if ((level) <= CONFIG_XIP_BLE_LOG_LEVEL) { char xb_line[96]; snprintf(xb_line, sizeof(xb_line), __VA_ARGS__); log_line(xb_line); } } while (0)
#else
#define XB_LOG(level, ...) do { } while (0)
#endif

/* Stack limits only; the caller chooses the values. */
#define ADV_DEFAULT_MIN_UNITS 0x20
#define ADV_DEFAULT_MAX_UNITS 0x40

/* ---------------------------------------------------------------- state (formerly fw/ble/src/lib.rs) */
static atomic_bool g_ready, g_connected, g_coc_connected, g_advertising, g_scanning, g_bonded, g_boot_enabled, g_suspended, g_released;
static atomic_uint g_generation, g_coc_rx, g_coc_rx_rejected, g_scan_reports, g_scan_matches;
static atomic_uint g_ingress_enqueued, g_ingress_accepted, g_ingress_rejected;
static atomic_uint g_egress_attempts, g_egress_sent, g_egress_errors, g_egress_blocked_pending;
static atomic_ushort g_handle = 0xffff;
static atomic_int g_last_scan_rssi;
static atomic_uchar g_last_scan_addr_type, g_local_addr_type, g_mode = XIP_BLE_MODE_ALWAYS;
static uint8_t g_last_scan_addr[6], g_local_addr[6];
static portMUX_TYPE g_state_mux = portMUX_INITIALIZER_UNLOCKED;   /* the two addresses and the scan cache */
static int g_link = -1;
#if CONFIG_XIP_BLE_SCAN_CACHE > 0
static uint8_t g_cache[CONFIG_XIP_BLE_SCAN_CACHE][XIP_BLE_SCAN_RECORD];
static uint8_t g_cache_n;
#endif

/* ---------------------------------------------------------------- mechanics (dmesh_nimble.c) */
static uint8_t s_addr_type;
static uint8_t s_addr[6];
static bool s_started;
static bool s_synced;
static bool s_adv_wanted;
static struct ble_npl_event s_adv_start_event;
static bool s_adv_start_event_ready;
/* GAP APIs are individually serialized by NimBLE, but replacing an
 * advertisement is a stop / configure / start transaction.  Firmware command
 * handling and companion polling run in different tasks, so protect the
 * whole transaction rather than allowing their individual GAP calls to
 * interleave. */
static SemaphoreHandle_t s_adv_lock;
static uint8_t s_adv_data[31];
static uint8_t s_adv_len;
static uint16_t s_adv_min = ADV_DEFAULT_MIN_UNITS;
static uint16_t s_adv_max = ADV_DEFAULT_MAX_UNITS;
static uint8_t s_scan_rsp[31];
static uint8_t s_scan_rsp_len;
static xip_nimble_filter s_filters[3];     /* the scan filters of the running (or last) scan */
static bool s_scan_wanted;
static bool s_scan_cancel_for_connect;
static struct ble_npl_event s_scan_start_event;
static bool s_scan_start_event_ready;
static uint32_t s_scan_duration;
static struct ble_npl_event s_scan_stop_event;
static bool s_scan_stop_event_ready;
static struct ble_npl_event s_adv_stop_event;
static bool s_adv_stop_event_ready;
static struct ble_npl_event s_service_stop_event;
static bool s_service_stop_event_ready;
static uint16_t s_conn_handle;
static bool s_connected;

#if XB_COC
/* A deliberately small, opt-in CoC transport.  The application creates the
 * server only after a `ble coc=true` command; normal GATT/rendezvous traffic
 * does not allocate a channel. */
/* CoC itself is reliable and segments/reassembles below this SDU size.  Keep
 * a normal QUIC-lite packet plus stream framing in one bounded application
 * buffer; 256 was only the original echo-test ceiling. */
/* CONFIG_XIP_BLE_COC_MTU / CONFIG_XIP_BLE_COC_BUFS are the upper limits; the first start chooses the values (the pool is sized then). */
static uint16_t s_coc_mtu;
static uint8_t s_coc_bufs;
#define DMESH_COC_MTU s_coc_mtu
#define DMESH_COC_BUF_COUNT s_coc_bufs
/* Allocated from the heap when the stack starts, so an image that never starts BLE holds none of it. */
static os_membuf_t *s_coc_mem;
static struct os_mempool s_coc_mempool;
static struct os_mbuf_pool s_coc_pool;
static bool s_coc_pool_ready;
static bool s_coc_server_started;
static uint16_t s_coc_psm;
static uint16_t s_coc_psm_requested;
static struct ble_l2cap_chan *s_coc_chan;
static struct ble_npl_event s_coc_start_event;
static bool s_coc_start_event_ready;
static struct ble_npl_event s_coc_tx_event;
static bool s_coc_tx_event_ready;
/* The pool packet being sent: the runtime lent it to link_tx; it goes back with xip_link_native_tx_done once the NimBLE host task has copied it
 * into an mbuf (NimBLE has no mbuf over external data, so that one copy is the latest point). */
static struct { uint8_t *data; uint16_t len; int slot; } s_tx;
static bool s_coc_tx_pending;
static SemaphoreHandle_t s_coc_tx_lock;
static struct ble_npl_event s_coc_connect_event;
static bool s_coc_connect_event_ready;
static ble_addr_t s_coc_connect_peer;
static uint16_t s_coc_connect_psm;
static uint16_t s_coc_connect_mtu;
static bool s_coc_connect_requested;
static void dmesh_coc_connect_if_requested(void);
#else
#define DMESH_COC_MTU 0
#endif

static int dmesh_gap_event(struct ble_gap_event *event, void *arg);
static int start_adv_now(void);
static int start_scan_now(uint32_t duration_ms);
static int stop_scan_now(void);
static int stop_adv_now(void);
static void stop_service_now(void);
static bool adv_matches_filter(const uint8_t *adv, uint8_t len);

/* ---------------------------------------------------------------- callbacks of the mechanics (formerly dmesh_nimble_on_*, in Rust) */
static void deliver_tx_ready(void)
{
    if (g_link >= 0) xip_link_native_ready(g_link);
}

static void deliver_state(bool connected)
{
    if (g_link >= 0) (void)xip_link_native_set_state(g_link, connected ? XIP_LINK_UP : XIP_LINK_DOWN, 0);
}

static void dmesh_nimble_on_ready(const uint8_t *addr, uint8_t addr_type)
{
    if (addr) {
        portENTER_CRITICAL(&g_state_mux);
        memcpy(g_local_addr, addr, 6);
        portEXIT_CRITICAL(&g_state_mux);
    }
    atomic_store(&g_local_addr_type, addr_type);
    atomic_store(&g_ready, true);
}

static void dmesh_nimble_on_connect(uint16_t handle)
{
    atomic_store(&g_handle, handle);
    atomic_fetch_add(&g_generation, 1);
    atomic_store(&g_connected, true);
}

static void dmesh_nimble_on_disconnect(uint16_t reason)
{
    (void)reason;
    atomic_store(&g_connected, false);
    atomic_store(&g_coc_connected, false);
    atomic_store(&g_handle, 0xffff);
}

static void dmesh_nimble_on_coc_state(uint8_t connected_in)
{
    const bool was_connected = atomic_load(&g_coc_connected);
    const bool connected = connected_in != 0;
    atomic_store(&g_coc_connected, connected);
    if (connected != was_connected) deliver_state(connected);
    if (!was_connected && connected) deliver_tx_ready();
}

static void dmesh_nimble_on_coc_tx_ready(void) { deliver_tx_ready(); }

/* One received SDU, straight into a pool packet of the runtime: the payload window follows XIP_PKT_HEADROOM, which stays empty so the
 * frame can be forwarded as IPv6/UDP without a copy. The runtime shows it to the common QUIC receive function and the link's
 * subscribers (xip_quic_rx_set, xip_runtime.h); an SDU of up to XIP_PKT_MAX bytes is carried whole. */
static void coc_receive(struct os_mbuf *rx)
{
    const uint16_t len = OS_MBUF_PKTLEN(rx);
    if (len == 0 || len > DMESH_COC_MTU || len > XIP_PKT_MAX || g_link < 0) {
        atomic_fetch_add(&g_coc_rx_rejected, 1);
        return;
    }
    atomic_fetch_add(&g_coc_rx, 1);
    if (!atomic_load(&g_coc_connected)) return;
    atomic_fetch_add(&g_ingress_enqueued, 1);
    xip_link_rx_buf buf;
    if (xip_link_native_rx_begin(g_link, &buf) != 0) {          /* nobody listens, or the pool is empty */
        atomic_fetch_add(&g_ingress_rejected, 1);
        return;
    }
    if (os_mbuf_copydata(rx, 0, len, buf.data) != 0) {
        xip_link_native_rx_abort(&buf);
        atomic_fetch_add(&g_ingress_rejected, 1);
        return;
    }
    xip_link_native_rx_commit(g_link, &buf, len, 0, 0);
    atomic_fetch_add(&g_ingress_accepted, 1);
}

static void dmesh_nimble_on_scan_state(uint8_t scanning) { atomic_store(&g_scanning, scanning != 0); }

static void dmesh_nimble_on_scan_result(uint8_t addr_type, const uint8_t *addr, int rssi, uint8_t matched)
{
    if (addr) {
        portENTER_CRITICAL(&g_state_mux);
        memcpy(g_last_scan_addr, addr, 6);
        portEXIT_CRITICAL(&g_state_mux);
    }
    atomic_store(&g_last_scan_addr_type, addr_type);
    atomic_store(&g_last_scan_rssi, rssi);
    atomic_fetch_add(&g_scan_reports, 1);
    if (matched) atomic_fetch_add(&g_scan_matches, 1);
#if CONFIG_XIP_BLE_SCAN_CACHE > 0
    if (addr) {
        portENTER_CRITICAL(&g_state_mux);
        uint8_t i = 0;
        while (i < g_cache_n && !(memcmp(g_cache[i], addr, 6) == 0 && g_cache[i][6] == addr_type)) i++;
        if (i == g_cache_n && g_cache_n < CONFIG_XIP_BLE_SCAN_CACHE) g_cache_n++;
        if (i < g_cache_n) {
            memcpy(g_cache[i], addr, 6);
            g_cache[i][6] = addr_type;
            g_cache[i][7] = (uint8_t)(int8_t)(rssi < -128 ? -128 : rssi > 127 ? 127 : rssi);
            g_cache[i][8] = (uint8_t)(g_cache[i][8] | (matched ? 1 : 0));
        }
        portEXIT_CRITICAL(&g_state_mux);
    }
#endif
}

static void dmesh_nimble_on_auth_complete(uint8_t bonded) { atomic_store(&g_bonded, bonded != 0); }

static void dmesh_adv_start_event_cb(struct ble_npl_event *event) {
    (void)event;
    /* GAP procedures must be started by the NimBLE host task. Firmware
     * command/rendezvous code only records the advertisement to publish. */
    if (s_adv_wanted) {
        int rc = start_adv_now();
        if (rc != 0) XB_LOG(1, "event type=ble.advertise state=failed rc=%d", rc);
    }
    s_adv_start_event_ready = true;
}

static void dmesh_scan_start_event_cb(struct ble_npl_event *event) {
    (void)event;
    if (s_scan_wanted) {
        int rc = start_scan_now(s_scan_duration);
        if (rc != 0 && rc != BLE_HS_EALREADY) {
            s_scan_wanted = false;
            dmesh_nimble_on_scan_state(0);
            XB_LOG(1, "event type=ble.scan state=start_failed rc=%d", rc);
        }
    }
    s_scan_start_event_ready = true;
}

static void dmesh_scan_stop_event_cb(struct ble_npl_event *event) {
    (void)event;
    if (s_scan_wanted) {
        int rc = stop_scan_now();
        if (rc != 0 && rc != BLE_HS_EALREADY) XB_LOG(1, "event type=ble.scan state=stop_failed rc=%d", rc);
    }
    s_scan_stop_event_ready = true;
}

static void dmesh_adv_stop_event_cb(struct ble_npl_event *event) {
    (void)event;
    (void)stop_adv_now();
    s_adv_stop_event_ready = true;
}

static void dmesh_service_stop_event_cb(struct ble_npl_event *event) {
    (void)event;
    stop_service_now();
    s_service_stop_event_ready = true;
}

#if XB_COC
static int dmesh_coc_recv_ready(struct ble_l2cap_chan *chan) {
    struct os_mbuf *sdu = os_mbuf_get_pkthdr(&s_coc_pool, 0);
    if (sdu == NULL) {
        return BLE_HS_ENOMEM;
    }
    return ble_l2cap_recv_ready(chan, sdu);
}

static int dmesh_coc_send_now(const uint8_t *data, uint16_t len) {
    if (data == NULL || len == 0 || len > DMESH_COC_MTU || s_coc_chan == NULL) {
        return ESP_ERR_INVALID_STATE;
    }
    struct os_mbuf *tx = os_mbuf_get_pkthdr(&s_coc_pool, 0);
    if (tx == NULL) {
        return BLE_HS_ENOMEM;
    }
    if (os_mbuf_append(tx, data, len) != 0) {
        os_mbuf_free_chain(tx);
        return BLE_HS_EMSGSIZE;
    }
    int rc = ble_l2cap_send(s_coc_chan, tx);
    if (rc != 0) {
        os_mbuf_free_chain(tx);
    }
    return rc;
}

static void tx_finish(int rc)
{
    const int slot = s_tx.slot;
    s_coc_tx_pending = false;
    if (rc == 0) atomic_fetch_add(&g_egress_sent, 1); else atomic_fetch_add(&g_egress_errors, 1);
    if (g_link >= 0) xip_link_native_tx_done(g_link, slot, rc == 0 ? XIP_OK : XIP_ERR_IO);
}

static void dmesh_coc_tx_event_cb(struct ble_npl_event *event) {
    (void)event;
    if (s_coc_tx_lock == NULL || xSemaphoreTake(s_coc_tx_lock, portMAX_DELAY) != pdTRUE) {
        return;
    }
    if (s_coc_tx_pending) {
        const int rc = dmesh_coc_send_now(s_tx.data, s_tx.len);
        if (rc != 0) XB_LOG(1, "event type=ble.coc state=tx_failed rc=%d", rc);
        tx_finish(rc);
    }
    xSemaphoreGive(s_coc_tx_lock);
    /* Accepted and failed submits both free the channel; the runtime tells the sender (READY) so it can offer the next packet. */
    dmesh_nimble_on_coc_tx_ready();
}

/* link_tx (any task): take the lent pool packet. 0: it is ours until tx_done; XIP_ERR_BUSY: the previous packet is still queued, the sender keeps this one. */
static int coc_tx_take(const xip_link_packet *pkt)
{
    if (!pkt || !pkt->len || pkt->len > DMESH_COC_MTU || !s_coc_tx_event_ready) return XIP_ERR_ARG;
    if (s_coc_tx_lock == NULL || xSemaphoreTake(s_coc_tx_lock, portMAX_DELAY) != pdTRUE) return XIP_ERR_IO;
    int rc = 0;
    if (s_coc_chan == NULL) rc = XIP_ERR_IO;
    else if (s_coc_tx_pending) rc = XIP_ERR_BUSY;
    else {
        s_tx.data = pkt->data; s_tx.len = (uint16_t)pkt->len; s_tx.slot = pkt->slot;
        s_coc_tx_pending = true;
    }
    xSemaphoreGive(s_coc_tx_lock);
    if (rc == 0) ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_coc_tx_event);
    return rc;
}

static int32_t dmesh_nimble_coc_tx_pending(void) {
    if (!s_started) {
        return ESP_OK;
    }
    return s_coc_tx_pending ? ESP_ERR_INVALID_STATE : ESP_OK;
}

static bool dmesh_nimble_coc_encrypted(void) {
    struct ble_gap_conn_desc desc;
    return s_connected && s_coc_chan != NULL
        && ble_gap_conn_find(s_conn_handle, &desc) == 0
        && desc.sec_state.encrypted;
}

static int dmesh_coc_event(struct ble_l2cap_event *event, void *arg) {
    (void)arg;
    switch (event->type) {
    case BLE_L2CAP_EVENT_COC_ACCEPT:
        return dmesh_coc_recv_ready(event->accept.chan);
    case BLE_L2CAP_EVENT_COC_CONNECTED:
        if (event->connect.status != 0) {
            XB_LOG(1, "event type=ble.coc state=connect_failed rc=%d psm=0x%04x", event->connect.status, s_coc_psm);
        } else {
            s_coc_chan = event->connect.chan;
            s_coc_connect_requested = false;
            dmesh_nimble_on_coc_state(1);
            XB_LOG(2, "event type=ble.coc state=connected");
        }
        return 0;
    case BLE_L2CAP_EVENT_COC_DATA_RECEIVED: {
        struct os_mbuf *rx = event->receive.sdu_rx;
        if (rx != NULL) {
            coc_receive(rx);
            os_mbuf_free_chain(rx);
        }
        return dmesh_coc_recv_ready(event->receive.chan);
    }
    case BLE_L2CAP_EVENT_COC_TX_UNSTALLED:
        if (event->tx_unstalled.status != 0) XB_LOG(1, "event type=ble.coc state=tx_unstalled_failed rc=%d", event->tx_unstalled.status);
        dmesh_nimble_on_coc_tx_ready();
        return 0;
    case BLE_L2CAP_EVENT_COC_DISCONNECTED:
        if (s_coc_chan == event->disconnect.chan) {
            s_coc_chan = NULL;
            s_coc_connect_requested = false;
            dmesh_nimble_on_coc_state(0);
            dmesh_nimble_on_coc_tx_ready();
        }
        XB_LOG(2, "event type=ble.coc state=disconnected");
        return 0;
    default:
        return 0;
    }
}

static void dmesh_coc_start_if_requested(void);

static void dmesh_coc_start_event_cb(struct ble_npl_event *event) {
    (void)event;
    /* The default queue is consumed by nimble_port_run(), i.e. the NimBLE
     * host task.  Do not create a listening PSM from the UART command task. */
    dmesh_coc_start_if_requested();
    s_coc_start_event_ready = true;
}

/* NimBLE host APIs must run on its host task. The firmware command handler
 * runs on the application task, so it records a request and the next NimBLE
 * callback creates the server. Calling ble_l2cap_create_server directly from
 * the command task can block the console behind the host lock. */
static void dmesh_coc_start_if_requested(void) {
    if (s_coc_server_started || s_coc_psm_requested == 0 || !s_coc_pool_ready) {
        return;
    }
    int rc = ble_l2cap_create_server(s_coc_psm_requested, DMESH_COC_MTU,
                                      dmesh_coc_event, NULL);
    if (rc == 0) {
        s_coc_psm = s_coc_psm_requested;
        s_coc_server_started = true;
        XB_LOG(2, "event type=ble.coc state=server_ready");
    } else {
        XB_LOG(1, "event type=ble.coc state=server_failed rc=%d psm=0x%04x", rc, s_coc_psm_requested);
    }
}

#if XB_CENTRAL
static void dmesh_coc_connect_event_cb(struct ble_npl_event *event) {
    (void)event;
    if (s_coc_connect_requested) {
        if (s_scan_wanted) {
            s_scan_cancel_for_connect = true;
            (void)stop_scan_now();
        }
        if (s_connected) {
            dmesh_coc_connect_if_requested();
        } else {
            int rc = ble_gap_connect(s_addr_type, &s_coc_connect_peer, 20000, NULL,
                                     dmesh_gap_event, NULL);
            if (rc != 0 && rc != BLE_HS_EALREADY) {
                XB_LOG(1, "event type=ble.connect state=failed rc=%d psm=0x%04x", rc, s_coc_connect_psm);
                s_coc_connect_requested = false;
            }
        }
    }
    s_coc_connect_event_ready = true;
}
#endif

static void dmesh_coc_connect_if_requested(void) {
#if XB_CENTRAL
    if (!s_coc_connect_requested || !s_connected || s_coc_chan != NULL || !s_coc_pool_ready) {
        return;
    }
    struct os_mbuf *sdu = os_mbuf_get_pkthdr(&s_coc_pool, 0);
    if (sdu == NULL) {
        XB_LOG(1, "event type=ble.coc state=connect_nomem psm=0x%04x", s_coc_connect_psm);
        s_coc_connect_requested = false;
        return;
    }
    int rc = ble_l2cap_connect(s_conn_handle, s_coc_connect_psm, s_coc_connect_mtu ? s_coc_connect_mtu : DMESH_COC_MTU,
                               sdu, dmesh_coc_event, NULL);
    if (rc != 0) {
        os_mbuf_free_chain(sdu);
        XB_LOG(1, "event type=ble.coc state=connect_failed rc=%d psm=0x%04x", rc, s_coc_connect_psm);
        s_coc_connect_requested = false;
    } else {
        s_coc_psm = s_coc_connect_psm;
    }
#endif
}
#endif /* XB_COC */

static int start_adv_now(void) {
#if XB_PERIPHERAL
    int rc;
    struct ble_gap_adv_params params = {0};
    if (!s_synced || s_adv_len == 0) {
        return 0;
    }

    if (s_adv_lock == NULL || xSemaphoreTake(s_adv_lock, portMAX_DELAY) != pdTRUE) {
        return ESP_ERR_INVALID_STATE;
    }

    rc = ble_gap_adv_stop();
    /* Stopping an already-idle advertiser is harmless. Report every other
     * failure with a distinct stage so the Android companion lab can diagnose
     * the transition without logcat. */
    /* Classic ESP32 reports EALREADY for an idle advertiser; ESP32-S3's
     * NimBLE host reports EINVAL for the same harmless stop. */
    if (rc != 0 && rc != BLE_HS_EALREADY && rc != BLE_HS_EINVAL) {
        xSemaphoreGive(s_adv_lock);
        return 1000 + rc;
    }

    rc = ble_gap_adv_set_data(s_adv_data, s_adv_len);
    if (rc != 0) {
        xSemaphoreGive(s_adv_lock);
        return 2000 + rc;
    }
    rc = s_scan_rsp_len ? ble_gap_adv_rsp_set_data(s_scan_rsp, s_scan_rsp_len) : 0;
    if (rc != 0) {
        xSemaphoreGive(s_adv_lock);
        return 3000 + rc;
    }

    params.conn_mode = BLE_GAP_CONN_MODE_UND;
    params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    params.itvl_min = s_adv_min;
    params.itvl_max = s_adv_max;

    rc = ble_gap_adv_start(s_addr_type, NULL, BLE_HS_FOREVER, &params,
                           dmesh_gap_event, NULL);
    if (rc == 0) {
        s_adv_wanted = true;
    }
    xSemaphoreGive(s_adv_lock);
    return rc == 0 ? 0 : 4000 + rc;
#else
    return ESP_ERR_NOT_SUPPORTED;
#endif
}

/* A filter matches when an AD structure of one of its types holds an element (`len` bytes) equal to its data. */
static bool adv_matches_filter(const uint8_t *adv, uint8_t len) {
    uint8_t offset = 0;
    while (offset + 1 < len) {
        uint8_t field_len = adv[offset];
        if (field_len < 2 || offset + field_len + 1 > len) {
            return false;
        }
        const uint8_t field_type = adv[offset + 1];
        const uint8_t *data = adv + offset + 2;
        const uint8_t data_len = field_len - 1;
        for (int i = 0; i < 3; i++) {
            const xip_nimble_filter *f = &s_filters[i];
            if (!f->len || f->len > sizeof(f->data) || (field_type != f->ad_types[0] && field_type != f->ad_types[1]) || !field_type) continue;
            for (uint8_t at = 0; at + f->len <= data_len; at = (uint8_t)(at + f->len)) {
                if (memcmp(data + at, f->data, f->len) == 0) return true;
            }
        }
        offset += field_len + 1;
    }
    return false;
}

static int start_scan_now(uint32_t duration_ms) {
#if XB_OBSERVER
    struct ble_gap_disc_params params;
    if (!s_synced) {
        return ESP_ERR_INVALID_STATE;
    }
    memset(&params, 0, sizeof(params));
    params.itvl = 0;
    params.window = 0;
    params.filter_policy = BLE_HCI_SCAN_FILT_NO_WL;
    params.limited = 0;
    params.passive = 1;
    params.filter_duplicates = 1;
    params.disable_observer_mode = 0;
    int rc = ble_gap_disc(s_addr_type, duration_ms, &params, dmesh_gap_event, NULL);
    if (rc == 0) {
        s_scan_wanted = true;
        dmesh_nimble_on_scan_state(1);
    }
    return rc;
#else
    (void)duration_ms;
    return ESP_ERR_NOT_SUPPORTED;
#endif
}

static int stop_scan_now(void) {
#if XB_OBSERVER
    if (!s_scan_wanted) {
        return 0;
    }
    int rc = ble_gap_disc_cancel();
    if (rc == 0 || rc == BLE_HS_EALREADY) {
        s_scan_wanted = false;
        dmesh_nimble_on_scan_state(0);
        return 0;
    }
    return rc;
#else
    return 0;
#endif
}

static int stop_adv_now(void) {
    if (s_adv_lock == NULL || xSemaphoreTake(s_adv_lock, portMAX_DELAY) != pdTRUE) {
        return ESP_ERR_INVALID_STATE;
    }
#if XB_PERIPHERAL
    ble_gap_adv_stop();
#endif
    xSemaphoreGive(s_adv_lock);
    return 0;
}

static void stop_service_now(void) {
    s_adv_wanted = false;
    (void)stop_scan_now();
    (void)stop_adv_now();
    if (s_connected) {
        (void)ble_gap_terminate(s_conn_handle, 0x13);
    }
#if XB_COC
    else if (s_coc_chan != NULL) {
        (void)ble_l2cap_disconnect(s_coc_chan);
    }
#endif
}

static int32_t dmesh_nimble_start_scan(uint32_t duration_ms) {
    if (!s_started) {
        return ESP_ERR_INVALID_STATE;
    }
    if (!XB_OBSERVER) {
        return ESP_ERR_NOT_SUPPORTED;
    }
    if (s_scan_wanted) {
        return 0;
    }
    s_scan_duration = duration_ms;
    s_scan_wanted = true;
    if (!s_scan_start_event_ready) {
        return 0;
    }
    s_scan_start_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_scan_start_event);
    return 0;
}

static int32_t dmesh_nimble_stop_scan(void) {
    if (!s_scan_wanted) {
        return 0;
    }
    if (!s_scan_stop_event_ready) {
        return 0;
    }
    s_scan_stop_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_scan_stop_event);
    return 0;
}

static int32_t dmesh_nimble_stop_service(void) {
    if (!s_started) {
        return ESP_ERR_INVALID_STATE;
    }
    s_adv_wanted = false;
    if (!s_service_stop_event_ready) {
        return 0;
    }
    s_service_stop_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_service_stop_event);
    return 0;
}

static int dmesh_gap_event(struct ble_gap_event *event, void *arg) {
    (void)arg;
#if XB_COC
    dmesh_coc_start_if_requested();
#endif
    /* Keep this callback deliberately small: it runs on the NimBLE host task.
     * Do not query the peer, restart advertising, emit console records, or
     * wake other radios here. */
    switch (event->type) {
#if XB_OBSERVER
    case BLE_GAP_EVENT_DISC:
        if (event->disc.length_data > 0 && event->disc.data != NULL) {
            uint8_t matched = adv_matches_filter(
                event->disc.data, event->disc.length_data) ? 1 : 0;
            dmesh_nimble_on_scan_result(event->disc.addr.type, event->disc.addr.val,
                                        event->disc.rssi, matched);
        }
        break;
    case BLE_GAP_EVENT_DISC_COMPLETE:
        s_scan_wanted = false;
        dmesh_nimble_on_scan_state(0);
#if XB_COC
        if (s_coc_connect_requested && !s_scan_cancel_for_connect) {
            s_coc_connect_requested = false;
            XB_LOG(2, "event type=ble.connect state=timeout");
        }
#endif
        s_scan_cancel_for_connect = false;
        XB_LOG(2, "event type=ble.scan state=complete");
        break;
#endif
    case BLE_GAP_EVENT_PARING_COMPLETE:
        if (event->pairing_complete.status == 0) {
            dmesh_nimble_on_auth_complete(1);
            XB_LOG(2, "event type=ble.auth state=complete bonded=1");
        } else {
            XB_LOG(1, "event type=ble.auth state=failed rc=%d", event->pairing_complete.status);
        }
        break;
    case BLE_GAP_EVENT_CONNECT:
        if (event->connect.status == 0) {
            s_conn_handle = event->connect.conn_handle;
            s_connected = true;
            dmesh_nimble_on_connect(event->connect.conn_handle);
            XB_LOG(2, "event type=ble.gap state=connected");
#if XB_COC
            dmesh_coc_connect_if_requested();
#endif
        } else {
#if XB_COC
            if (s_coc_connect_requested) {
                s_coc_connect_requested = false;
            }
#endif
            XB_LOG(1, "event type=ble.gap state=connect_failed rc=%d", event->connect.status);
        }
        break;
    case BLE_GAP_EVENT_DISCONNECT:
        s_connected = false;
        s_conn_handle = 0xffff;
#if XB_COC
        s_coc_connect_requested = false;
#endif
        dmesh_nimble_on_disconnect(event->disconnect.reason);
        XB_LOG(2, "event type=ble.gap state=disconnected reason=%d", event->disconnect.reason);
        /* LE advertising stops while a central owns the GAP connection.  CoC
         * is intentionally short-lived, so retain the requested rendezvous
         * advertisement for the next Android connection after disconnect. */
        if (s_adv_wanted) {
            int rc = start_adv_now();
            if (rc != 0) {
                XB_LOG(1, "event type=ble.advertise state=restart_failed rc=%d", rc);
            } else {
                XB_LOG(2, "event type=ble.advertise state=restarted");
            }
        }
        break;
    default:
        break;
    }
    return 0;
}

static void on_stack_reset(int reason) {
    s_synced = false;
    s_adv_wanted = false;
    s_scan_wanted = false;
    s_scan_cancel_for_connect = false;
    s_connected = false;
    s_conn_handle = 0xffff;
#if XB_COC
    s_coc_chan = NULL;
    s_coc_server_started = false;
    if (s_coc_tx_pending) tx_finish(XIP_ERR_IO);
    s_coc_connect_requested = false;
    s_coc_connect_event_ready = true;
#endif
    dmesh_nimble_on_disconnect(0);
    dmesh_nimble_on_scan_state(0);
#if XB_COC
    dmesh_nimble_on_coc_state(0);
#endif
    XB_LOG(1, "event type=nimble.reset reason=%d", reason);
}

static void on_stack_sync(void) {
    int rc = ble_hs_util_ensure_addr(0);
    if (rc == 0) {
        rc = ble_hs_id_infer_auto(0, &s_addr_type);
    }
    if (rc == 0) {
        rc = ble_hs_id_copy_addr(s_addr_type, s_addr, NULL);
    }
    s_synced = rc == 0;
    if (s_synced) {
        dmesh_nimble_on_ready(s_addr, s_addr_type);
#if XB_COC
        dmesh_coc_start_if_requested();
#endif
        if (s_adv_wanted) {
            start_adv_now();
        }
    } else {
        XB_LOG(1, "event type=nimble.sync ok=false rc=%d", rc);
    }
}

static void nimble_host_task(void *param) {
    (void)param;
    nimble_port_run();
    nimble_port_freertos_deinit();
}

static int32_t dmesh_nimble_init(uint16_t coc_mtu, uint8_t coc_bufs) {
    if (s_started) {
        return 0;
    }
#if XB_COC
    s_coc_mtu = coc_mtu;
    s_coc_bufs = coc_bufs;
#else
    (void)coc_mtu; (void)coc_bufs;
#endif

    int rc = nimble_port_init();
    if (rc != ESP_OK) {
        return rc;
    }

    ble_svc_gap_init();
    ble_svc_gap_device_name_set("xip");       /* the caller names the device in start */
    ble_svc_gatt_init();

    /* CoC-only companion mode deliberately has no DMesh GATT application
     * service.  ATT/GATT remains part of the BLE stack, but the only public
     * rendezvous is advertising and all DMesh payloads use an LE CoC channel.
     * This avoids retaining a notify subscription simply to carry data. */

    ble_hs_cfg.reset_cb = on_stack_reset;
    ble_hs_cfg.sync_cb = on_stack_sync;
    /* Establish basic ATT/GATT for CoC negotiation.  DMesh does not expose a
     * GATT payload or bond-management control surface. */
    ble_hs_cfg.sm_bonding = 1;
    ble_hs_cfg.sm_mitm = 0;
    ble_hs_cfg.sm_sc = 1;
    ble_hs_cfg.sm_sec_lvl = 2;
    ble_hs_cfg.sm_io_cap = BLE_HS_IO_NO_INPUT_OUTPUT;
    ble_hs_cfg.sm_our_key_dist = BLE_HS_KEY_DIST_ENC_KEY | BLE_HS_KEY_DIST_ID_KEY;
    ble_hs_cfg.sm_their_key_dist = BLE_HS_KEY_DIST_ENC_KEY | BLE_HS_KEY_DIST_ID_KEY;

#if XB_COC
    if (!s_coc_mem) {
        s_coc_mem = heap_caps_malloc(OS_MEMPOOL_SIZE(DMESH_COC_BUF_COUNT, DMESH_COC_MTU) * sizeof(os_membuf_t), MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
        if (!s_coc_mem) {
            s_coc_mem = NULL;
            nimble_port_deinit();
            return ESP_ERR_NO_MEM;
        }
    }
    rc = os_mempool_init(&s_coc_mempool, DMESH_COC_BUF_COUNT, DMESH_COC_MTU,
                         s_coc_mem, "dmesh_coc");
    if (rc != 0) {
        return rc;
    }
    rc = os_mbuf_pool_init(&s_coc_pool, &s_coc_mempool, DMESH_COC_MTU,
                           DMESH_COC_BUF_COUNT);
    if (rc != 0) {
        return rc;
    }
    s_coc_pool_ready = true;
    ble_npl_event_init(&s_coc_start_event, dmesh_coc_start_event_cb, NULL);
    s_coc_start_event_ready = true;
    s_coc_tx_lock = xSemaphoreCreateMutex();
    if (s_coc_tx_lock == NULL) {
        return ESP_ERR_NO_MEM;
    }
    ble_npl_event_init(&s_coc_tx_event, dmesh_coc_tx_event_cb, NULL);
    s_coc_tx_event_ready = true;
#if XB_CENTRAL
    ble_npl_event_init(&s_coc_connect_event, dmesh_coc_connect_event_cb, NULL);
    s_coc_connect_event_ready = true;
#endif
#endif

    s_adv_lock = xSemaphoreCreateMutex();
    if (s_adv_lock == NULL) {
        return ESP_ERR_NO_MEM;
    }
    ble_npl_event_init(&s_adv_start_event, dmesh_adv_start_event_cb, NULL);
    s_adv_start_event_ready = true;
    ble_npl_event_init(&s_adv_stop_event, dmesh_adv_stop_event_cb, NULL);
    s_adv_stop_event_ready = true;
    ble_npl_event_init(&s_scan_start_event, dmesh_scan_start_event_cb, NULL);
    s_scan_start_event_ready = true;
    ble_npl_event_init(&s_scan_stop_event, dmesh_scan_stop_event_cb, NULL);
    s_scan_stop_event_ready = true;
    ble_npl_event_init(&s_service_stop_event, dmesh_service_stop_event_cb, NULL);
    s_service_stop_event_ready = true;

    /* ESP-IDF owns controller enablement and host-task affinity.  The manual
     * xTaskCreate path can run nimble_port_run without enabling the controller
     * and stalls the application/UART task on classic ESP32. */
    nimble_port_freertos_init(nimble_host_task);
    s_started = true;
    XB_LOG(2, "event type=nimble.init ok=true");
    return 0;
}

static int32_t dmesh_nimble_start_coc_server(uint16_t psm) {
#if XB_COC
    if (!s_started || !s_coc_pool_ready) {
        return ESP_ERR_INVALID_STATE;
    }
    if (psm < 0x0080 || psm > 0x00ff) {
        /* The Android public client API accepts application dynamic LE PSMs
         * in this range.  IPSP's assigned PSM is intentionally not exposed
         * by this raw echo probe. */
        return ESP_ERR_INVALID_ARG;
    }
    if (s_coc_server_started) {
        return psm == s_coc_psm ? 0 : ESP_ERR_INVALID_STATE;
    }
    s_coc_psm_requested = psm;
    if (!s_coc_start_event_ready) {
        return ESP_ERR_INVALID_STATE;
    }
    /* The listener itself is installed only by the NimBLE host task.  Queue
     * that work after sync; the UART command may take several seconds while
     * the controller starts, so callers must use the BLE command timeout. */
    if (s_synced) {
        s_coc_start_event_ready = false;
        ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_coc_start_event);
    }
    return 0;
#else
    (void)psm;
    return ESP_ERR_NOT_SUPPORTED;
#endif
}

static int32_t dmesh_nimble_connect_coc(const uint8_t *peer_addr, uint8_t peer_addr_type, uint16_t psm, uint16_t mtu) {
#if XB_COC && XB_CENTRAL
    if (peer_addr == NULL || !s_started || !s_coc_pool_ready || !s_coc_connect_event_ready) {
        return ESP_ERR_INVALID_STATE;
    }
    if (psm < 0x0080 || psm > 0x00ff) {
        return ESP_ERR_INVALID_ARG;
    }
    if (s_coc_chan != NULL) {
        return 0;
    }
    s_coc_connect_peer.type = peer_addr_type;
    memcpy(s_coc_connect_peer.val, peer_addr, 6);
    s_coc_connect_psm = psm;
    s_coc_connect_mtu = mtu;
    s_coc_connect_requested = true;
    s_coc_connect_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_coc_connect_event);
    return 0;
#else
    (void)peer_addr;
    (void)peer_addr_type;
    (void)psm;
    (void)mtu;
    return ESP_ERR_NOT_SUPPORTED;
#endif
}

#if !XB_COC
static int32_t dmesh_nimble_coc_tx_pending(void) {
    return ESP_OK;
}

static bool dmesh_nimble_coc_encrypted(void) {
    return false;
}
#endif

static int dmesh_nimble_clear_bonds(void) {
    /* Forget every stored peer key: a peer bonded before this call has to pair
     * again, and cannot reconnect with an old long-term key. */
    return ble_store_clear();
}

static int32_t dmesh_nimble_start_advertising(const uint8_t *adv, uint8_t adv_len,
                                       uint16_t min_units, uint16_t max_units) {
    if (adv == NULL || adv_len == 0 || adv_len > sizeof(s_adv_data)) {
        return ESP_ERR_INVALID_ARG;
    }
    memcpy(s_adv_data, adv, adv_len);
    s_adv_len = adv_len;
    s_adv_min = min_units;
    s_adv_max = max_units < min_units ? min_units : max_units;
    s_adv_wanted = true;
    if (!s_started || !s_adv_start_event_ready) {
        return ESP_ERR_INVALID_STATE;
    }
    s_adv_start_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_adv_start_event);
    return 0;
}

static int32_t dmesh_nimble_stop_advertising(void) {
    s_adv_wanted = false;
    if (!s_started || !s_adv_stop_event_ready) {
        return 0;
    }
    s_adv_stop_event_ready = false;
    ble_npl_eventq_put(nimble_port_get_dflt_eventq(), &s_adv_stop_event);
    return 0;
}

/* ---------------------------------------------------------------- the service on top (formerly fw/ble/src/lib.rs) */
static int xerr(int rc)
{
    switch (rc) {
    case 0: return XIP_OK;
    case ESP_ERR_INVALID_ARG: return XIP_ERR_ARG;
    case ESP_ERR_NOT_SUPPORTED: return XIP_ERR_UNSUPPORTED;
    case ESP_ERR_NO_MEM: return XIP_ERR_NOMEM;
    case ESP_ERR_INVALID_STATE: return XIP_ERR_BUSY;
    default: return XIP_ERR_IO;
    }
}

/* Start the stack (the first call fixes the CoC pool), then the CoC server and the advertisement the caller describes. */
static int start_service(const xip_nimble_start *r)
{
    if (atomic_load(&g_released)) return XIP_ERR_UNSUPPORTED;
    if (r->adv_len > sizeof(r->adv) || r->scan_rsp_len > sizeof(r->scan_rsp) || r->name_len >= sizeof(r->name)) return XIP_ERR_ARG;
#if XB_COC
    if ((r->psm || !s_started) && (!r->coc_mtu || r->coc_mtu > CONFIG_XIP_BLE_COC_MTU || r->coc_buffers < 3 || r->coc_buffers > CONFIG_XIP_BLE_COC_BUFS)) return XIP_ERR_ARG;
#endif
    if (!atomic_load(&g_ready)) {
        int rc = dmesh_nimble_init(r->coc_mtu, r->coc_buffers);
        if (rc != 0) return xerr(rc);
    }
    if (r->name_len) {
        char name[sizeof(r->name)];
        memcpy(name, r->name, r->name_len);
        name[r->name_len] = 0;
        (void)ble_svc_gap_device_name_set(name);
    }
    int rc;
    if (r->psm) {
        rc = xerr(dmesh_nimble_start_coc_server(r->psm));
        if (rc != 0) return rc;
    }
    if (!r->adv_len) return XIP_OK;
    memcpy(s_scan_rsp, r->scan_rsp, r->scan_rsp_len);
    s_scan_rsp_len = r->scan_rsp_len;
    rc = dmesh_nimble_start_advertising(r->adv, r->adv_len, r->adv_min ? r->adv_min : ADV_DEFAULT_MIN_UNITS, r->adv_max ? r->adv_max : ADV_DEFAULT_MAX_UNITS);
    if (rc != 0) return xerr(rc);
    atomic_store(&g_advertising, true);
    return XIP_OK;
}

static int stop_service(void)
{
    int rc = dmesh_nimble_stop_service();
    if (rc == 0) {
        atomic_store(&g_advertising, false);
        atomic_store(&g_scanning, false);
    }
    return xerr(rc);
}

bool xip_ble_coc_tx_pending(void)
{
    return atomic_load(&g_ready) && dmesh_nimble_coc_tx_pending() != 0;
}

bool xip_ble_coc_connected(void) { return atomic_load(&g_coc_connected); }


/* ---------------------------------------------------------------- the table */
static int c_start(void *ctx, const xip_nimble_start *r)
{
    (void)ctx;
    if (!r || r->size < sizeof(*r)) return XIP_ERR_ARG;
    return start_service(r);
}

static int c_stop(void *ctx) { (void)ctx; return stop_service(); }

static int c_scan(void *ctx, const xip_nimble_scan *r)
{
    (void)ctx;
    if (!r || r->size < sizeof(*r)) return XIP_ERR_ARG;
    if (!atomic_load(&g_ready)) return XIP_ERR_TIMEOUT;
    const uint32_t ms = r->duration_ms;
    if (!ms) return XIP_ERR_ARG;
    memcpy(s_filters, r->filters, sizeof(s_filters));
#if CONFIG_XIP_BLE_SCAN_CACHE > 0
    portENTER_CRITICAL(&g_state_mux);
    g_cache_n = 0;
    memset(g_cache, 0, sizeof(g_cache));
    portEXIT_CRITICAL(&g_state_mux);
#endif
    int rc = dmesh_nimble_start_scan(ms);
    if (rc == 0) atomic_store(&g_scanning, true);
    return xerr(rc);
}

static int c_scan_stop(void *ctx)
{
    (void)ctx;
    int rc = dmesh_nimble_stop_scan();
    if (rc == 0) atomic_store(&g_scanning, false);
    return xerr(rc);
}

static int c_status(void *ctx, xip_ble_status *s)
{
    (void)ctx;
    if (!s || s->size < sizeof(*s)) return XIP_ERR_ARG;
    const uint32_t size = s->size;
    memset(s, 0, sizeof(*s));
    s->size = size;
    s->ready = atomic_load(&g_ready);
    s->advertising = atomic_load(&g_advertising);
    s->connected = atomic_load(&g_connected);
    s->coc_connected = atomic_load(&g_coc_connected);
    s->scanning = atomic_load(&g_scanning);
    s->bonded = atomic_load(&g_bonded);
    s->coc_tx_pending = xip_ble_coc_tx_pending();
    s->last_scan_addr_type = atomic_load(&g_last_scan_addr_type);
    s->local_addr_type = atomic_load(&g_local_addr_type);
    s->encrypted = dmesh_nimble_coc_encrypted();
    s->mode = atomic_load(&g_mode);
    s->boot_enabled = atomic_load(&g_boot_enabled);
    s->suspended = atomic_load(&g_suspended);
    s->generation = atomic_load(&g_generation);
    s->handle = atomic_load(&g_handle);
    s->coc_rx = atomic_load(&g_coc_rx);
    s->coc_rx_rejected = atomic_load(&g_coc_rx_rejected);
    s->scan_reports = atomic_load(&g_scan_reports);
    s->scan_matches = atomic_load(&g_scan_matches);
    s->last_scan_rssi = atomic_load(&g_last_scan_rssi);
    portENTER_CRITICAL(&g_state_mux);
    memcpy(s->last_scan_addr, g_last_scan_addr, 6);
    memcpy(s->local_addr, g_local_addr, 6);
    portEXIT_CRITICAL(&g_state_mux);
    s->ingress_enqueued = atomic_load(&g_ingress_enqueued);
    s->ingress_accepted = atomic_load(&g_ingress_accepted);
    s->ingress_rejected = atomic_load(&g_ingress_rejected);
    s->egress_attempts = atomic_load(&g_egress_attempts);
    s->egress_sent = atomic_load(&g_egress_sent);
    s->egress_send_errors = atomic_load(&g_egress_errors);
    s->egress_blocked_pending = atomic_load(&g_egress_blocked_pending);
    return XIP_OK;
}

static int c_connect(void *ctx, const xip_nimble_connect *r)
{
    (void)ctx;
    if (!r || r->size < sizeof(*r)) return XIP_ERR_ARG;
    if (!atomic_load(&g_ready)) return XIP_ERR_TIMEOUT;
    if (r->psm < 0x0080 || r->psm > 0x00ff || r->coc_mtu > DMESH_COC_MTU) return XIP_ERR_ARG;
    return xerr(dmesh_nimble_connect_coc(r->addr, r->addr_type, r->psm, r->coc_mtu));
}

static int c_bonds_clear(void *ctx)
{
    (void)ctx;
    return dmesh_nimble_clear_bonds() == 0 ? XIP_OK : XIP_ERR_IO;
}

static int c_mode(void *ctx, const xip_ble_mode *r)
{
    (void)ctx;
    if (!r || r->size < sizeof(*r) || r->mode > XIP_BLE_MODE_RENDEZVOUS || r->boot > XIP_BLE_BOOT_ON) return XIP_ERR_ARG;
    if (r->mode != XIP_BLE_MODE_KEEP) atomic_store(&g_mode, r->mode);
    if (r->boot != XIP_BLE_BOOT_KEEP) atomic_store(&g_boot_enabled, r->boot == XIP_BLE_BOOT_ON);
    return XIP_OK;
}

static int c_scan_results(void *ctx, xip_ble_scan_results *r)
{
    (void)ctx;
    if (!r || r->size < sizeof(*r)) return XIP_ERR_ARG;
    const uint32_t size = r->size;
    memset(r, 0, sizeof(*r));
    r->size = size;
#if CONFIG_XIP_BLE_SCAN_CACHE > 0
    portENTER_CRITICAL(&g_state_mux);
    const uint8_t n = g_cache_n > XIP_BLE_SCAN_RESULT_MAX ? XIP_BLE_SCAN_RESULT_MAX : g_cache_n;
    memcpy(r->results, g_cache, (size_t)n * XIP_BLE_SCAN_RECORD);
    r->results_len = (uint8_t)(n * XIP_BLE_SCAN_RECORD);
    portEXIT_CRITICAL(&g_state_mux);
#endif
    return XIP_OK;
}

/* Wake/sleep. A live link is a transfer in progress and always vetoes a sleep (the design note: the ESP may stay awake while a CoC connection
 * transfers, and must not retain BLE or the CPU awake after it ends). In `always` mode a live service vetoes too: advertising or scanning keeps
 * the controller from sleeping on the boards, so the veto reports the truth. In `rendezvous` mode advertising and scanning are stopped here, on
 * the NimBLE host task and waited for, and re-armed by resume, so the device is discoverable only in its awake windows. */
static bool s_resume_adv;

static int c_suspend(void *ctx)
{
    (void)ctx;
    if (!s_started || atomic_load(&g_released)) return XIP_OK;
    if (atomic_load(&g_connected) || atomic_load(&g_coc_connected)) return XIP_ERR_BUSY;
    const bool live = s_adv_wanted || s_scan_wanted;
    if (atomic_load(&g_mode) != XIP_BLE_MODE_RENDEZVOUS) return live ? XIP_ERR_BUSY : XIP_OK;
    if (atomic_load(&g_suspended)) return XIP_OK;
    s_resume_adv = s_adv_wanted;
    (void)dmesh_nimble_stop_advertising();
    (void)dmesh_nimble_stop_scan();
    atomic_store(&g_advertising, false);
    atomic_store(&g_scanning, false);
    /* the controller must be idle before the sleep: wait for the host task to have stopped both */
    for (int i = 0; i < 100 && (ble_gap_adv_active() || ble_gap_disc_active()); i++) vTaskDelay(1);
    atomic_store(&g_suspended, true);
    return XIP_OK;
}

static int c_resume(void *ctx)
{
    (void)ctx;
    if (!atomic_load(&g_suspended)) return XIP_OK;
    atomic_store(&g_suspended, false);
    if (s_resume_adv) {
        s_resume_adv = false;
        int rc = dmesh_nimble_start_advertising(s_adv_data, s_adv_len, s_adv_min, s_adv_max);
        if (rc == 0 || rc == ESP_ERR_INVALID_STATE) atomic_store(&g_advertising, true);   /* INVALID_STATE: a start is already queued */
    }
    return XIP_OK;
}

static const xip_nimble nimble_component = {
    .version = XIP_NIMBLE_VERSION, .size = sizeof(xip_nimble),
    .start = c_start, .stop = c_stop, .scan = c_scan, .status = c_status, .scan_stop = c_scan_stop, .connect = c_connect,
    .bonds_clear = c_bonds_clear, .mode = c_mode, .scan_results = c_scan_results,
    .suspend = c_suspend, .resume = c_resume,
};

const xip_nimble *xip_ble_component(void) { return atomic_load(&g_released) ? NULL : &nimble_component; }

static int power_suspend(void) { return c_suspend(NULL); }
static int power_resume(void) { return c_resume(NULL); }
static const xip_power_ble power_ble = {.suspend = power_suspend, .resume = power_resume};

static int link_register(void);

int xip_ble_install(void)
{
    int rc = xip_component_register("nimble", &nimble_component);
    if (rc != XIP_OK) return rc;
    xip_power_set_ble(&power_ble);
    /* the CoC channel is the native link "ble" (mtu = the SDU size) */
    if (link_register() < 0 && XB_COC) return XIP_ERR_NOMEM;
    return XIP_OK;
}


#if XB_COC
static int link_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    (void)ctx;
    (void)dest;
    atomic_fetch_add(&g_egress_attempts, 1);
    const int rc = coc_tx_take(pkt);
    if (rc == XIP_ERR_BUSY) atomic_fetch_add(&g_egress_blocked_pending, 1);     /* the sender keeps the packet and waits for READY */
    else if (rc != 0) atomic_fetch_add(&g_egress_errors, 1);
    return rc;      /* 0: the packet is ours until xip_link_native_tx_done */
}

static int link_register(void)
{
    if (g_link >= 0) return g_link;
    int addr = xip_link_native_register("ble", CONFIG_XIP_BLE_COC_MTU, 0, link_tx, NULL);
    if (addr < 0) return addr;
    g_link = addr;
    if (atomic_load(&g_coc_connected)) (void)xip_link_native_set_state(addr, XIP_LINK_UP, 0);
    return addr;
}
#else
static int link_register(void) { return XIP_ERR_UNSUPPORTED; }
#endif

void xip_ble_configure_boot(bool enabled)
{
    atomic_store(&g_boot_enabled, enabled);
#if CONFIG_XIP_BLE_RELEASE_WHEN_OFF
    if (!enabled && !s_started && !atomic_load(&g_released)) {
        /* The image will not use BLE in this boot: give the controller's memory back to the heap. */
        if (esp_bt_mem_release(ESP_BT_MODE_BLE) == ESP_OK) atomic_store(&g_released, true);
    }
#endif
}

bool xip_ble_boot_enabled(void) { return atomic_load(&g_boot_enabled); }

#else /* CONFIG_XIP_BLE is off or there is no NimBLE: the component is a stub and offers no table */

const xip_nimble *xip_ble_component(void) { return NULL; }
int xip_ble_install(void) { return XIP_ERR_UNSUPPORTED; }
bool xip_ble_coc_connected(void) { return false; }
bool xip_ble_coc_tx_pending(void) { return false; }
void xip_ble_configure_boot(bool enabled) { (void)enabled; }
bool xip_ble_boot_enabled(void) { return false; }

#endif
