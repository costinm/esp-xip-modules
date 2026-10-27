#include <stdatomic.h>
/* SPDX-License-Identifier: Apache-2.0 */
/* The event-driven host (xip-abi crate docs, Model): one worker task calls the handler of every module for each event
 * (start, stop, request, interrupt, alarm). Instance lifecycle, alarms, and the core calls of the host table: log,
 * alloc, time, settings. Peripherals are in xip_periph.c. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "xip_psa.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "esp_attr.h"
#include "nvs.h"
#include "esp_flash.h"
#include "esp_netif.h"
#include "lwip/sockets.h"
#include <errno.h>
#include "xip_cbor.h"
#include "internal.h"
#include "xip_wifi.h"

#include "packet_queue.inc"
static packet_delivery_queue packet_queue;

static const char *TAG = "xip";
static xip_host_hooks_t hooks;
static bool inited;
static volatile bool net_connected;
static QueueHandle_t worker_queue;
static TaskHandle_t worker_task;
static struct xip_instance *registry[XIP_MAX_INSTANCES];
static uint32_t next_id = 1;
static portMUX_TYPE registry_lock = portMUX_INITIALIZER_UNLOCKED;
static xip_alarm_ctx_t alarm_ctx[XIP_MAX_INSTANCES * XIP_ALARMS];

#define SETTING_KEY_MAX 15
#define SETTING_VALUE_MAX 256

static struct xip_instance *I(void *user) { return user; }
static int dispatch(struct xip_instance *in, int component, int method, int value, uint32_t flags, const uint8_t *data, size_t len);
static int dispatch_packet(struct xip_instance *in, int component, int method, const uint8_t *data, size_t len, uint32_t pkt,
                           uint32_t from, uint64_t id, uint32_t flags, int32_t value, uint32_t time_us, const xip_udp_address *src);
static const xip_link *link_table(void);
static const xip_quic *quic_module_table(void);
static const xip_stream *stream_table(void);
static void streams_release(uint32_t owner);
static void quic_module_release(struct xip_instance *in);
static struct xip_instance *find(uint32_t id);
static void links_release(struct xip_instance *in);
static int link_send(struct xip_instance *in, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t token);
static int quic_reply_prefix(uint32_t dst, uint64_t token, uint64_t id, int slot, uint16_t *start, uint16_t *len);
static void quic_reply_finish(uint32_t dst, uint64_t token, uint64_t id);
static void serial_release(struct xip_instance *in);
static int h_tcp_accept(void *u, uint16_t port, uint32_t mode, int32_t c, int32_t m, uint32_t *handle);
static void stream_accept(struct xip_instance *in);
static xip_pool *pool;
static xip_pool *pool_get(void);
static const xip_quic_rx_ops *_Atomic quic_rx;      /* xip_quic_rx_set */
static const xip_quic_local_ops *_Atomic quic_local;   /* xip_quic_local_set */
static void quic_module_event(uint32_t addr,uint32_t kind,int32_t value,uint64_t id);
static _Atomic uint32_t quic_module_owner;
static _Atomic bool quic_classifier;                    /* xip_quic_classifier_enable */
static inline bool quic_active(void) { return atomic_load(&quic_rx) || atomic_load(&quic_local) || atomic_load(&quic_classifier) || atomic_load(&quic_module_owner); }
static void drop_slot(int slot);
static void packet_rx_storage_shortage(void);
static uint32_t now_us32(void);
static void link_fanout(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us, const xip_udp_address *src);
static bool quic_common_rx(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us, const xip_udp_address *src);
#ifndef QUIC_CONSOLE_COMPONENT
#define QUIC_CONSOLE_COMPONENT 20                /* answers console and native envelope methods (mod_con) */
#endif
static struct xip_instance *by_component(int component, const struct xip_instance *except);
static bool known_locked(const struct xip_instance *in);
static int dispatch_x(struct xip_instance *in, const xip_call *call);

static int h_log(void *user, const uint8_t *data, size_t len)
{
    struct xip_instance *in = I(user);
    if (!data) return XIP_ERR_ARG;
    char line[97];
    size_t n = len < sizeof(line) - 1 ? len : sizeof(line) - 1;
    memcpy(line, data, n);
    line[n] = 0;
    in->stats.logged++;
    /* WARN level: INFO is usually compiled out of a production Main, and a module's log is its only
     * way to explain a failure. */
    ESP_LOGW(TAG, "%s: %s%s", in->name, line, n == len ? "" : "...");
    return n == len ? XIP_OK : XIP_ERR_SIZE;
}

static uint64_t h_now_us(void *user) { (void)user; return (uint64_t)esp_timer_get_time(); }
static uint64_t h_now_ms(void *user) { (void)user; return (uint64_t)(esp_timer_get_time() / 1000); }

int xip_wifi_set_intent(const xip_wifi_intent *i, uint32_t *gen) { return hooks.wifi_set ? hooks.wifi_set(i, gen) : XIP_ERR_UNSUPPORTED; }
int xip_wifi_get_intent(xip_wifi_intent *i, uint32_t *gen) { return hooks.wifi_get ? hooks.wifi_get(i, gen) : XIP_ERR_UNSUPPORTED; }
int xip_wifi_get_tuning(xip_wifi_tuning *t) { return hooks.wifi_tuning_get ? hooks.wifi_tuning_get(t) : XIP_ERR_UNSUPPORTED; }

static atomic_bool wifi_pending, wake_pending;
static atomic_int wake_value;
static atomic_uint wifi_generation;

/* get_component("wifi"): no context argument; usable from any task. */
static int c_wifi_set(void *ctx, const xip_wifi_intent *i, uint32_t *gen)
{
    (void)ctx;
    uint32_t ignored;
    if (!i || i->size < sizeof(*i)) return XIP_ERR_ARG;
    return xip_wifi_set_intent(i, gen ? gen : &ignored);
}
static int c_wifi_get(void *ctx, xip_wifi_intent *i, uint32_t *gen)
{
    (void)ctx;
    uint32_t ignored;
    if (!i || i->size < sizeof(*i)) return XIP_ERR_ARG;
    return xip_wifi_get_intent(i, gen ? gen : &ignored);
}
static int c_wifi_state(void *ctx, xip_wifi_state *st)
{
    (void)ctx;
    if (!st || st->size < 52) return XIP_ERR_ARG;
    return hooks.wifi_state ? hooks.wifi_state(st) : XIP_ERR_UNSUPPORTED;
}
_Static_assert(sizeof(xip_ap_t) == sizeof(xip_wifi_ap), "the scan record of the host and of the component");
static int c_wifi_scan(void *ctx, xip_wifi_ap *out, size_t max)
{
    (void)ctx;
    if (!out || !max) return XIP_ERR_ARG;
    return hooks.wifi_scan ? hooks.wifi_scan((xip_ap_t *)out, max) : XIP_ERR_UNSUPPORTED;
}
static int c_wifi_monitor(void *ctx, const xip_wifi_monitor *m)
{
    (void)ctx;
    if (!m || m->size < sizeof(*m)) return XIP_ERR_ARG;
    return hooks.wifi_monitor ? hooks.wifi_monitor(m) : XIP_ERR_UNSUPPORTED;
}
static int c_wifi_reconnect(void *ctx, bool enabled) {
    (void)ctx; return hooks.wifi_reconcile ? xip_wifi_auto_reconnect(enabled) : XIP_ERR_UNSUPPORTED;
}
static int c_wifi_scan_channels(void *ctx, xip_wifi_ap *out, size_t max, uint16_t channels, uint16_t dwell_ms) {
    (void)ctx; return hooks.wifi_reconcile ? xip_wifi_scan_channels((xip_ap_t *)out, max, channels, dwell_ms) : XIP_ERR_UNSUPPORTED;
}
static const xip_wifi wifi_component = {
    .version = 2, .size = sizeof(xip_wifi), .set = c_wifi_set, .get = c_wifi_get, .state = c_wifi_state,
    .scan = c_wifi_scan, .monitor = c_wifi_monitor,
    .auto_reconnect = c_wifi_reconnect, .scan_channels = c_wifi_scan_channels,
};

/* get_component("power") */
static int c_power_sleep(void *ctx, const xip_power_sleep *r) { return xip_power_request(r, I(ctx)->id); }
static int c_power_info(void *ctx, xip_power_info *i) { (void)ctx; return xip_power_get_info(i); }
static int c_power_dump_locks(void *ctx) { (void)ctx; return xip_power_dump(0); }
static int c_power_dump_tasks(void *ctx) { (void)ctx; return xip_power_dump(1); }
static int c_power_until(void *ctx, uint64_t at, uint32_t flags) { return xip_power_until(I(ctx)->id, at, flags); }
static int c_power_cancel(void *ctx) { return xip_power_cancel(I(ctx)->id); }
static int c_power_activate(void *ctx, uint32_t ms) { (void)ctx; if (ms) xip_power_activate(ms); else xip_power_clear_activity(); return 0; }
static int c_power_awake_for(void *ctx, uint32_t ms) { (void)ctx; return xip_power_awake_for(ms); }
/* One activity grace for every bearer, after full CBOR decoding. A radio
 * adapter has already established local direction before using this call. */
static int c_power_received(void *ctx, const uint8_t *data, size_t len, uint32_t directed)
{
    (void)ctx;
    if (!xip_envelope_activity(data,len,directed==1)) return 0;
    xip_envelope env;
    (void)xip_envelope_parse(data,len,&env);
    xip_cbor_r target;
    /* Routing-to metadata is unsupported here: never vote on an unselected route. */
    if (xip_cbor_get(data,len,4,&target)==0) return 0;
    if (env.component==1 && env.method==4 && xip_cbor_get(env.fields,env.fields_len,25,&target)==0) {
        xip_cbor_item it;
        xip_wifi_state actual={.size=sizeof(actual)};
        if (xip_cbor_next(&target,&it) || it.type!=XIP_CBOR_BYTES || it.n!=6
            || !hooks.wifi_state || hooks.wifi_state(&actual)<0
            || (memcmp(it.data,actual.sta_mac,6) && memcmp(it.data,actual.ap_mac,6))) return 0;
    }
    xip_power_activate(5000);
    return 1;
}
static const xip_power power_component = {
    .version = 4, .size = sizeof(xip_power), .sleep = c_power_sleep, .info = c_power_info,
    .dump_locks = c_power_dump_locks, .dump_tasks = c_power_dump_tasks,
    .sleep_until = c_power_until, .cancel = c_power_cancel, .activate = c_power_activate, .awake_for = c_power_awake_for, .received=c_power_received,
};

/* get_component("wifi-tuning") */
static int c_tuning_set(void *ctx, const xip_wifi_tuning *t)
{
    (void)ctx;
    if (!t || t->size < sizeof(*t)) return XIP_ERR_ARG;
    return hooks.wifi_tuning_set ? hooks.wifi_tuning_set(t) : XIP_ERR_UNSUPPORTED;
}
static int c_tuning_get(void *ctx, xip_wifi_tuning *t)
{
    (void)ctx;
    if (!t || t->size < sizeof(*t)) return XIP_ERR_ARG;
    return xip_wifi_get_tuning(t);
}
static const xip_wifi_tuner tuner_component = {
    .version = 1, .size = sizeof(xip_wifi_tuner), .set = c_tuning_set, .get = c_tuning_get,
};

static int h_heap_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!out || !capacity) return XIP_ERR_ARG;
    return xip_heap_info(out, capacity);
}

static int h_nv(void *user, const uint8_t *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!key || !key_len || key_len > 32 || (value_len && !value) || (!value && (!out || !capacity))) return XIP_ERR_ARG;
    /* Shared device policy, independent of the application's Wi-Fi/NV hook.
     * mod_flash reads this only on activation; there is no live-mode switch. */
    if (key_len == 10 && !memcmp(key, "flash.auth", 10)) {
        if (value && value_len &&
            !((value_len == 6 && !memcmp(value, "strict", 6)) ||
              (value_len == 8 && !memcmp(value, "unsigned", 8)))) return XIP_ERR_ARG;
        nvs_handle_t ns;
        esp_err_t rc = nvs_open("wl", value ? NVS_READWRITE : NVS_READONLY, &ns);
        char mode[9] = "strict";
        if (rc != ESP_OK) {
            if (value || rc != ESP_ERR_NVS_NOT_FOUND) return XIP_ERR_IO;
        } else {
            if (value) {
                if (value_len) {
                    memcpy(mode, value, value_len); mode[value_len] = 0;
                    rc = nvs_set_str(ns, "flash_auth", mode);
                } else rc = nvs_erase_key(ns, "flash_auth");
                if (rc == ESP_ERR_NVS_NOT_FOUND) rc = ESP_OK;
                if (rc == ESP_OK) rc = nvs_commit(ns);
            } else {
                size_t n = sizeof(mode);
                rc = nvs_get_str(ns, "flash_auth", mode, &n);
                if (rc == ESP_ERR_NVS_NOT_FOUND) { memcpy(mode, "strict", 7); rc = ESP_OK; }
            }
            nvs_close(ns);
            if (rc != ESP_OK) return XIP_ERR_IO;
        }
        if (value) return XIP_OK;
        size_t n = strlen(mode);
        if (n > capacity) return XIP_ERR_SIZE;
        memcpy(out, mode, n);
        return (int)n;
    }
    if (!hooks.nv) return XIP_ERR_UNSUPPORTED;
    int rc = hooks.nv((const char *)key, key_len, value, value_len, out, capacity);
    return rc >= 0 ? rc : rc == -1 ? XIP_ERR_ARG : rc == -3 ? XIP_ERR_IO : rc == -5 ? XIP_ERR_SIZE : XIP_ERR_NOTFOUND;
}

static int h_heap_dump(void *user, int kind) { (void)user; return xip_heap_dump(kind); }

static int open_ns(struct xip_instance *in, nvs_open_mode_t mode, nvs_handle_t *h)
{
    char ns[NVS_NS_NAME_MAX_SIZE];
    snprintf(ns, sizeof(ns), "em_%.11s", in->name);
    esp_err_t e = nvs_open(ns, mode, h);
    if (e == ESP_ERR_NVS_NOT_FOUND) return XIP_ERR_NOTFOUND;
    return e == ESP_OK ? 0 : XIP_ERR_IO;
}

static bool key_of(const uint8_t *key, size_t n, char out[SETTING_KEY_MAX + 1])
{
    if (!key || n == 0 || n > SETTING_KEY_MAX) return false;
    memcpy(out, key, n);
    out[n] = 0;
    return true;
}

static int h_get_setting(void *user, const uint8_t *key, size_t key_len, uint8_t *value, size_t cap, size_t *value_len)
{
    char k[SETTING_KEY_MAX + 1];
    if (!key_of(key, key_len, k) || !value_len || (cap && !value)) return XIP_ERR_ARG;
    nvs_handle_t h;
    int rc = open_ns(I(user), NVS_READONLY, &h);
    if (rc) return rc;
    size_t n = cap;
    esp_err_t e = nvs_get_blob(h, k, value, &n);
    if (e == ESP_ERR_NVS_INVALID_LENGTH) { /* ask the real size */
        e = nvs_get_blob(h, k, NULL, &n);
        *value_len = n;
        nvs_close(h);
        return e == ESP_OK ? XIP_ERR_SIZE : XIP_ERR_IO;
    }
    nvs_close(h);
    if (e == ESP_ERR_NVS_NOT_FOUND) return XIP_ERR_NOTFOUND;
    if (e != ESP_OK) return XIP_ERR_IO;
    *value_len = n;
    return 0;
}

static int h_set_setting(void *user, const uint8_t *key, size_t key_len, const uint8_t *value, size_t value_len)
{
    char k[SETTING_KEY_MAX + 1];
    if (!key_of(key, key_len, k) || (value_len && !value) || value_len > SETTING_VALUE_MAX) return XIP_ERR_ARG;
    nvs_handle_t h;
    int rc = open_ns(I(user), NVS_READWRITE, &h);
    if (rc) return rc;
    esp_err_t e = value_len ? nvs_set_blob(h, k, value, value_len) : nvs_erase_key(h, k);
    if (e == ESP_ERR_NVS_NOT_FOUND && !value_len) e = ESP_OK;
    if (e == ESP_OK) e = nvs_commit(h);
    nvs_close(h);
    return e == ESP_OK ? 0 : XIP_ERR_IO;
}


/* ---- alarms: the timers only queue events; the alarm state belongs to the worker ---- */
static uint32_t fire_delay_ms(uint32_t delay, uint32_t window, uint32_t flags)
{
    if (!(flags & XIP_ALARM_FLEXIBLE) || !hooks.next_window_ms) return delay;
    uint32_t w = hooks.next_window_ms(hooks.window_ctx);
    if (w == 0) return delay;
    if (w >= delay && w - delay <= window) return w;   /* the next window is inside the allowed range */
    return w < delay ? delay : delay + window;         /* none inside it: as late as allowed, batching with others */
}

static void alarm_cb(void *arg)
{
    const xip_alarm_ctx_t *c = arg;
    xip_event_t ev = {.kind = XIP_EV_ALARM, .inst = c->inst, .seq = c->seq, .alarm = c->index};
    (void)xQueueSend(worker_queue, &ev, 0);
}

static void alarm_arm(struct xip_instance *in, int idx, uint32_t after_ms)
{
    xip_alarm_t *a = &in->alarms[idx];
    xip_alarm_ctx_t *ctx = &alarm_ctx[in->slot * XIP_ALARMS + idx];
    esp_timer_stop(a->timer);
    a->seq++;
    ctx->inst = in->id; ctx->seq = a->seq; ctx->index = (uint8_t)idx;
    if (esp_timer_start_once(a->timer, (uint64_t)after_ms * 1000) != ESP_OK) a->used = false;
}

static int h_alarm_set(void *user, uint32_t delay_ms, uint32_t window_ms, uint32_t interval_ms, uint32_t flags,
                       int component, int method)
{
    struct xip_instance *in = I(user);
    if (flags & ~(uint32_t)(XIP_ALARM_FLEXIBLE | XIP_ALARM_CONNECTED | XIP_ALARM_WINDOW)) return XIP_ERR_ARG;
    int idx = -1;
    for (int i = 0; i < XIP_ALARMS && idx < 0; i++) if (!in->alarms[i].used) idx = i;
    if (idx < 0) return XIP_ERR_NOMEM;
    xip_alarm_t *a = &in->alarms[idx];
    if (!a->timer) {
        const esp_timer_create_args_t ta = {.callback = alarm_cb, .arg = &alarm_ctx[in->slot * XIP_ALARMS + idx], .name = "xip_alarm"};
        if (esp_timer_create(&ta, &a->timer) != ESP_OK) return XIP_ERR_NOMEM;
    }
    a->used = true; a->waiting_net = false;
    a->window_ms = window_ms; a->interval_ms = interval_ms; a->flags = flags;
    a->component = component; a->method = method;
    a->due_us = esp_timer_get_time() + (int64_t)delay_ms * 1000;
    alarm_arm(in, idx, fire_delay_ms(delay_ms, window_ms, flags));
    return a->used ? idx : XIP_ERR_IO;
}

static int h_alarm_cancel(void *user, int alarm)
{
    struct xip_instance *in = I(user);
    if (alarm < 0 || alarm >= XIP_ALARMS || !in->alarms[alarm].used) return XIP_ERR_NOTFOUND;
    esp_timer_stop(in->alarms[alarm].timer);
    in->alarms[alarm].used = false;
    in->alarms[alarm].seq++;
    return XIP_OK;
}


/* ---- ports: virtual, numbered, and not tied to a link or an encoding ----
 * A module's manifest names a port (key 9); the runtime binds it when the module is created, and the module only handles calls. Any link
 * can deliver frames to a port, and the frame arrives with `from` = the link and `id` = the token of the far end, which the module answers
 * to as for any link frame. Datagram ports are packets and messages: the serial line serves them (PPP records that carry their port), UDP will. Stream ports (the manifest's second setting) are streams the module reads with socket_read: TCP, QUIC
 * streams later. The serial driver runs while a module has the console port, and is released in the background when it ends. */
#define XIP_PORTS_MAX 8
static struct { uint32_t port; int component; } ports[XIP_PORTS_MAX];
static int serial_link = -1;

static void serial_release(struct xip_instance *in)
{
    /* the serial stays: it serves every module's port records, not only the one that bound it */
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_PORTS_MAX; i++)
        if (ports[i].port == in->port && ports[i].component == in->component)
            memset(&ports[i], 0, sizeof(ports[i]));
    portEXIT_CRITICAL(&registry_lock);
}

/* The far end of the serial link is a port: the token of a frame is the port it came to, and a frame sent to it goes out as that port's. */
static int serial_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    (void)ctx;
    /* Written synchronously: DONE releases the driver reference. A failed write is an error, as for every link: the
     * runtime returns the packet to the pool and counts the drop; the next record starts with its own flag and is fine. */
    if (xip_serial_write_frame(dest ? (uint32_t)dest : XIP_PORT_CONSOLE, pkt->data, pkt->len) != 0) return XIP_ERR_IO;
    return XIP_LINK_TX_DONE;
}

static bool port_bound(uint32_t port)
{
    bool bound = false;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_PORTS_MAX; i++) bound |= ports[i].port == port;
    portEXIT_CRITICAL(&registry_lock);
    return bound;
}

/* A frame for `port` without a copy: begin returns 0 and a pool packet to fill (capacity XIP_PKT_MAX, with the headroom in front, as every
 * packet), 1 when no module has the port (skip the frame), or an error (XIP_ERR_NOMEM: the pool is empty). Then commit or abort. */
static int port_rx_begin(uint32_t port, xip_link_rx_buf *buf, bool cached)
{
    xip_pool *p = cached ? pool : pool_get();
    if (!p || !buf) return XIP_ERR_ARG;
    if (!port_bound(port) && !(port == XIP_PORT_QUIC && quic_active()) && !hooks.request_start) return 1;
    int slot = cached ? xip_pool_acquire_cached(p, 0) : xip_pool_acquire(p, 0);
    if (slot < 0) { packet_rx_storage_shortage(); return XIP_ERR_NOMEM; }
    buf->slot = slot;
    buf->data = xip_pool_slot(p, slot) + XIP_PKT_HEADROOM;
    buf->capacity = XIP_PKT_MAX;
    return XIP_OK;
}

int xip_port_rx_begin(uint32_t port, xip_link_rx_buf *buf) { return port_rx_begin(port, buf, false); }
int xip_port_rx_begin_cached(uint32_t port, xip_link_rx_buf *buf) { return port_rx_begin(port, buf, true); }

void xip_port_rx_commit_radio(int link, uint32_t port, const xip_link_rx_buf *buf,
    size_t len, uint64_t token, int32_t signal, uint32_t time_us)
{
    if (len > XIP_PKT_MAX || link < 0) { drop_slot(buf->slot); return; }
    link_fanout((uint32_t)link, port, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, token, signal, time_us, NULL);
}

void xip_port_rx_commit(int link, uint32_t port, const xip_link_rx_buf *buf, size_t len, uint64_t token)
{
    if (len > XIP_PKT_MAX || link < 0) { drop_slot(buf->slot); return; }
    link_fanout((uint32_t)link, port, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, token, 0, now_us32(), NULL);
}

/* A datagram of a runtime-owned UDP port: not a link, so `link` is the socket handle
 * (`ADDR_UDP + fd`) the module received on `call.from` and sends back through `udp_send`,
 * and `src` rides with the deliver as the call's source. */
void xip_port_rx_commit_udp(uint32_t link, uint32_t port, const xip_link_rx_buf *buf, size_t len, const xip_udp_address *src)
{
    if (len > XIP_PKT_MAX) { drop_slot(buf->slot); return; }
    link_fanout(link, port, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, 0, 0, now_us32(), src);
}

void xip_port_rx_abort(const xip_link_rx_buf *buf) { drop_slot(buf->slot); }

int xip_serial_rx_begin(uint32_t port, xip_link_rx_buf *buf) { return xip_port_rx_begin(port, buf); }
void xip_serial_rx_commit(uint32_t port, const xip_link_rx_buf *buf, size_t len) { xip_port_rx_commit(serial_link, port, buf, len, port); }
void xip_serial_rx_abort(const xip_link_rx_buf *buf) { xip_port_rx_abort(buf); }

static int port_bind(struct xip_instance *in)
{
    if (!in->component) return XIP_ERR_ARG;
    int rc = XIP_OK;
    portENTER_CRITICAL(&registry_lock);
    int free_at = -1;
    bool already_bound = false;
    for (int i = 0; i < XIP_PORTS_MAX; i++) {
        if (ports[i].port == in->port && ports[i].component != in->component) rc = XIP_ERR_BUSY;
        if (ports[i].port == in->port && ports[i].component == in->component) already_bound = true;
        if (!ports[i].port && free_at < 0) free_at = i;
    }
    if (!rc && !already_bound && free_at < 0) rc = XIP_ERR_NOMEM;
    if (!rc && !already_bound) { ports[free_at].port = in->port; ports[free_at].component = in->component; }
    portEXIT_CRITICAL(&registry_lock);
    if (rc) return rc;
    /* the serial link was registered at xip_host_init: binding a port only routes records to it */
    return XIP_OK;
}

static struct xip_instance *by_component(int component, const struct xip_instance *except)
{
    struct xip_instance *found = NULL;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_MAX_INSTANCES && !found; i++)
        if (registry[i] && registry[i] != except && registry[i]->component == component) found = registry[i];
    portEXIT_CRITICAL(&registry_lock);
    return found;
}

/* A manifest may assign additional wire components to one module. These do
 * not change its lifecycle/event address or require the debug console. */
static struct xip_instance *envelope_target(int component)
{
    struct xip_instance *found = NULL;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_MAX_INSTANCES && !found; i++) {
        struct xip_instance *in = registry[i];
        if (!in) continue;
        for (int c = 0; c < XIP_COMPONENTS_MAX; c++)
            if (in->envelope_components[c] == component) { found = in; break; }
        if (!found && in->component == component) found = in;
    }
    portEXIT_CRITICAL(&registry_lock);
    return found;
}

int xip_device_info(uint8_t *out, size_t capacity) { return xip_rt_device_info(&hooks, out, capacity); }
static int h_device_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user; return xip_device_info(out, capacity);
}
static int h_info_publish(void *user, int32_t component, int32_t method, const uint8_t *data, size_t len)
{
    return xip_rt_info_publish(I(user)->id, component, method, data, len);
}

/* Optional components of other ESP-IDF components (xip_ble): they register a table by name, so the runtime does not link them. */
#define XIP_EXTRA_COMPONENTS 4
static struct { const char *name; const void *table; } extra_components[XIP_EXTRA_COMPONENTS];
int xip_component_register(const char *name, const void *table)
{
    if (!name || !table) return XIP_ERR_ARG;
    for (int i = 0; i < XIP_EXTRA_COMPONENTS; i++) {
        if (extra_components[i].name && strcmp(extra_components[i].name, name) == 0) { extra_components[i].table = table; return XIP_OK; }
    }
    for (int i = 0; i < XIP_EXTRA_COMPONENTS; i++) {
        if (!extra_components[i].name) { extra_components[i].table = table; extra_components[i].name = name; return XIP_OK; }
    }
    return XIP_ERR_NOMEM;
}


/* ---- components hosted by a module (manifest key 11) ----
 * A module that hosts a component answers HOST_TABLE with its table and context (xip_component_publish, right after HOST_START). The table is
 * `{u32 version, u32 size, then one function pointer per method}`, every function `int f(void *ctx, ...)` taking at most five more words (integers
 * and pointers). get_component(name) hands a caller a COPY whose functions are runtime thunks: each takes `exec_lock`, the lock that also serializes
 * every module handler (so the module's state, host table and allocator cell are never used by two tasks), calls the module's function with the
 * module's context instead of the caller's, and returns XIP_ERR_NOTFOUND when no running module hosts the name any more. The copy and its thunks
 * are kept for the life of the system, so a table held across a module restart works again once the module is back. A module's worker must not
 * call another module through such a table (it runs inline on the worker, under the same lock); other tasks may. */
static SemaphoreHandle_t exec_lock;
static StaticSemaphore_t exec_lock_buf;
#define XIP_THUNKS 24
#define XIP_THUNK_TABLES 4
static struct { char name[16]; uint8_t *copy; } thunk_tables[XIP_THUNK_TABLES];
static struct { int8_t table; uint8_t entry; } thunk_slots[XIP_THUNKS];      /* table -1: free */

static struct xip_instance *provider_locked(const char *name, int *at)
{
    struct xip_instance *found = NULL;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_MAX_INSTANCES && !found; i++) {
        struct xip_instance *p = registry[i];
        if (!p || p->finishing) continue;
        for (int c = 0; c < XIP_COMPONENTS_MAX; c++)
            if (p->comps[c].table && !strcmp(p->comps[c].name, name)) { found = p; *at = c; break; }
    }
    portEXIT_CRITICAL(&registry_lock);
    return found;
}

static int thunk_run(unsigned slot, uintptr_t a1, uintptr_t a2, uintptr_t a3, uintptr_t a4, uintptr_t a5)
{
    xSemaphoreTakeRecursive(exec_lock, portMAX_DELAY);
    int rc = XIP_ERR_NOTFOUND;
    const int t = thunk_slots[slot].table;
    int c = 0;
    struct xip_instance *p = t >= 0 ? provider_locked(thunk_tables[t].name, &c) : NULL;
    if (p) {
        const uint8_t *tab = p->comps[c].table;
        uint32_t size = 0;
        memcpy(&size, tab + 4, sizeof size);
        const size_t off = 8 + (size_t)thunk_slots[slot].entry * sizeof(void *);
        void *fn = NULL;
        if (off + sizeof(void *) <= size) memcpy(&fn, tab + off, sizeof fn);
        rc = fn ? ((int (*)(void *, uintptr_t, uintptr_t, uintptr_t, uintptr_t, uintptr_t))fn)(p->comps[c].ctx, a1, a2, a3, a4, a5)
                : XIP_ERR_UNSUPPORTED;
    }
    xSemaphoreGiveRecursive(exec_lock);
    return rc;
}
#define XIP_THUNK(n) static int thunk_##n(void *ctx, uintptr_t a1, uintptr_t a2, uintptr_t a3, uintptr_t a4, uintptr_t a5) \
    { (void)ctx; return thunk_run(n, a1, a2, a3, a4, a5); }
XIP_THUNK(0) XIP_THUNK(1) XIP_THUNK(2) XIP_THUNK(3) XIP_THUNK(4) XIP_THUNK(5) XIP_THUNK(6) XIP_THUNK(7)
XIP_THUNK(8) XIP_THUNK(9) XIP_THUNK(10) XIP_THUNK(11) XIP_THUNK(12) XIP_THUNK(13) XIP_THUNK(14) XIP_THUNK(15)
XIP_THUNK(16) XIP_THUNK(17) XIP_THUNK(18) XIP_THUNK(19) XIP_THUNK(20) XIP_THUNK(21) XIP_THUNK(22) XIP_THUNK(23)
static void *const thunk_fns[XIP_THUNKS] = {
    thunk_0, thunk_1, thunk_2, thunk_3, thunk_4, thunk_5, thunk_6, thunk_7, thunk_8, thunk_9, thunk_10, thunk_11,
    thunk_12, thunk_13, thunk_14, thunk_15, thunk_16, thunk_17, thunk_18, thunk_19, thunk_20, thunk_21, thunk_22, thunk_23,
};

static void components_publish(struct xip_instance *in);
static bool settle(struct xip_instance *in, int rc);
static int dispatch(struct xip_instance *in, int component, int method, int value, uint32_t flags, const uint8_t *data, size_t len);

/* HOST_START, once, then the tables the module hosts. By the worker's start event, or inline by get_component (a module started on demand must have its
 * table before the caller gets an answer). A failed start ends the instance. Under exec_lock. */
static int instance_start_result(struct xip_instance *in)
{
    xSemaphoreTakeRecursive(exec_lock,portMAX_DELAY);
    int result=0;
    if(!in->start_done){
        in->start_done=true;
        int rc=dispatch(in,XIP_HOST_COMPONENT,XIP_HOST_START,0,0,in->start_request,in->start_len);
        if(settle(in,rc))result=rc<0?rc:XIP_ERR_NOTFOUND;
        else components_publish(in);
    }
    xSemaphoreGiveRecursive(exec_lock);return result;
}
static bool instance_start(struct xip_instance *in){return instance_start_result(in)==0;}
/* Resolve and initialize on the serialized owner, retaining the original call. */
static struct xip_instance *request_target(int component,uint32_t port,int *error)
{
    xSemaphoreTakeRecursive(exec_lock,portMAX_DELAY);
    struct xip_instance *in=NULL;
    if(component>0)in=envelope_target(component);
    else{
        portENTER_CRITICAL(&registry_lock);
        for(unsigned i=0;i<XIP_MAX_INSTANCES && !in;i++)if(registry[i] && registry[i]->port==port)in=registry[i];
        portEXIT_CRITICAL(&registry_lock);
    }
    int rc=in?0:XIP_ERR_NOTFOUND;
    if(!in && hooks.request_start){
        rc=hooks.request_start(component,port);
        if(!rc){
            if(component>0)in=envelope_target(component);
            else{portENTER_CRITICAL(&registry_lock);for(unsigned i=0;i<XIP_MAX_INSTANCES && !in;i++)if(registry[i] && registry[i]->port==port)in=registry[i];portEXIT_CRITICAL(&registry_lock);}
            if(!in)rc=XIP_ERR_NOTFOUND;
        }
    }
    if(!in && rc==XIP_ERR_NOTFOUND && component>0 && component!=QUIC_CONSOLE_COMPONENT){
        in=by_component(QUIC_CONSOLE_COMPONENT,NULL);
        if(!in && hooks.request_start){rc=hooks.request_start(QUIC_CONSOLE_COMPONENT,0);if(!rc)in=by_component(QUIC_CONSOLE_COMPONENT,NULL);}
    }
    if(in){rc=instance_start_result(in);if(rc<0)in=NULL;}
    *error=in?0:rc<0?rc:XIP_ERR_NOTFOUND;
    xSemaphoreGiveRecursive(exec_lock);return in;
}

/* The table of `name` for a caller, or NULL: the runtime's copy of the table of the running module that hosts it. */
static const void *component_of_module(const char *name)
{
    if (!exec_lock) return NULL;
    xSemaphoreTakeRecursive(exec_lock, portMAX_DELAY);
    const void *result = NULL;
    int c = 0;
    struct xip_instance *p = provider_locked(name, &c);
    if (!p && hooks.component_start && hooks.component_start(name) == XIP_OK) {
        /* installed but not running: it is running now (created); give it its HOST_START here so that its table exists before we answer */
        struct xip_instance *h = NULL;
        portENTER_CRITICAL(&registry_lock);
        for (int i = 0; i < XIP_MAX_INSTANCES && !h; i++)
            for (int k = 0; registry[i] && k < XIP_COMPONENTS_MAX; k++)
                if (!strcmp(registry[i]->comps[k].name, name) && !registry[i]->start_done) { h = registry[i]; break; }
        portEXIT_CRITICAL(&registry_lock);
        if (h) instance_start(h);
        p = provider_locked(name, &c);
    }
    if (p) {
        int t = -1, freet = -1;
        for (int i = 0; i < XIP_THUNK_TABLES; i++) {
            if (thunk_tables[i].copy && !strcmp(thunk_tables[i].name, name)) t = i;
            if (!thunk_tables[i].copy && freet < 0) freet = i;
        }
        if (t >= 0) result = thunk_tables[t].copy;
        else if (freet >= 0 && strlen(name) < sizeof(thunk_tables[0].name)) {
            const uint8_t *tab = p->comps[c].table;
            uint32_t size = 0;
            memcpy(&size, tab + 4, sizeof size);
            const size_t n = size >= 8 ? (size - 8) / sizeof(void *) : 0;
            unsigned need = 0;
            for (size_t e = 0; e < n; e++) { void *fn; memcpy(&fn, tab + 8 + e * sizeof fn, sizeof fn); need += fn != NULL; }
            unsigned avail = 0;
            for (int i = 0; i < XIP_THUNKS; i++) avail += thunk_slots[i].table < 0;
            uint8_t *copy = need <= avail && size <= 8 + XIP_THUNKS * sizeof(void *) ? calloc(1, size) : NULL;
            if (copy) {
                memcpy(copy, tab, 8);
                int next = 0;
                for (size_t e = 0; e < n; e++) {
                    void *fn; memcpy(&fn, tab + 8 + e * sizeof fn, sizeof fn);
                    if (!fn) continue;
                    while (thunk_slots[next].table >= 0) next++;
                    thunk_slots[next].table = (int8_t)freet; thunk_slots[next].entry = (uint8_t)e;
                    void *thunk = thunk_fns[next];
                    memcpy(copy + 8 + e * sizeof thunk, &thunk, sizeof thunk);
                }
                strlcpy(thunk_tables[freet].name, name, sizeof(thunk_tables[freet].name));
                thunk_tables[freet].copy = copy;
                result = copy;
            }
        }
    }
    xSemaphoreGiveRecursive(exec_lock);
    return result;
}

/* Right after HOST_START (worker, exec_lock held): ask the module for the table of each component its manifest names. The answer is the request's
 * response (xip_instance_request's machinery, run inline): the table and the context as two native words. */
static void components_publish(struct xip_instance *in)
{
    if (!in->comps[0].name[0]) return;
    if (!in->response) in->response = malloc(CONFIG_XIP_RESPONSE_MAX);
    if (!in->response) return;
    for (int c = 0; c < XIP_COMPONENTS_MAX && in->comps[c].name[0]; c++) {
        in->in_request = true; in->responded = false; in->response_len = 0;
        const xip_call call = {.size = sizeof(call), .component = XIP_HOST_COMPONENT, .method = XIP_HOST_TABLE, .flags = 1,
                               .data = (const uint8_t *)in->comps[c].name, .len = strlen(in->comps[c].name), .from = XIP_ADDR_MAIN, .id = XIP_REQUEST_ID};
        (void)dispatch_x(in, &call);
        in->in_request = false;
        if (in->responded && in->response_len == 2 * sizeof(void *)) {
            const void *table; void *ctx;
            memcpy(&table, in->response, sizeof table);
            memcpy(&ctx, in->response + sizeof table, sizeof ctx);
            uint32_t size = 0;
            if (table) memcpy(&size, (const uint8_t *)table + 4, sizeof size);
            if (table && size >= 8) {
                portENTER_CRITICAL(&registry_lock);
                in->comps[c].table = table; in->comps[c].ctx = ctx;
                portEXIT_CRITICAL(&registry_lock);
            } else ESP_LOGW(TAG, "%s: bad table for component %s", in->name, in->comps[c].name);
        } else ESP_LOGW(TAG, "%s: no table for component %s", in->name, in->comps[c].name);
    }
}

static const void *h_get_component(void *user, const char *name)
{
    (void)user; /* Requester context; native component functions take no extra argument. */
    extern const xip_psa *xip_psa_component(void);
    extern const xip_identity *xip_identity_component(void);
    if (!name) return NULL;
    if (!strcmp(name, "serial")) return xip_serial_component();
    if (strcmp(name, "psa") == 0) return xip_psa_component();
    if (strcmp(name, "identity") == 0) return xip_identity_component();
    if (strcmp(name, "link") == 0) return link_table();
    if (strcmp(name, "quic") == 0) return quic_module_table();
    if (strcmp(name, "stream") == 0) return stream_table();
    if (strcmp(name, "power") == 0) return &power_component;
    if (strcmp(name, "hw") == 0) return xip_periph_table();
    if (strcmp(name, "wifi") == 0) return hooks.wifi_set ? &wifi_component : NULL;
    if (strcmp(name, "wifi-tuning") == 0) return hooks.wifi_tuning_set ? &tuner_component : NULL;
    for (int i = 0; i < XIP_EXTRA_COMPONENTS; i++) {
        if (extra_components[i].name && strcmp(extra_components[i].name, name) == 0) return extra_components[i].table;
    }
    return component_of_module(name);       /* a running module that hosts it (manifest key 11) */
}

static int h_modules_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.modules_info) return XIP_ERR_UNSUPPORTED;
    if (!out || !capacity) return XIP_ERR_ARG;
    return hooks.modules_info(out, capacity);
}

static int h_module_start(void *user, const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len)
{
    (void)user;
    if (!hooks.module_start) return XIP_ERR_UNSUPPORTED;
    if (!name || !name_len || (request_len && !request)) return XIP_ERR_ARG;
    return hooks.module_start(name, name_len, request, request_len);
}

static int h_module_stop(void *user, const uint8_t *name, size_t name_len)
{
    (void)user;
    if (!hooks.module_stop) return XIP_ERR_UNSUPPORTED;
    if (!name || !name_len) return XIP_ERR_ARG;
    return hooks.module_stop(name, name_len);
}

static int h_module_manifest(void *user, const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.module_manifest) return XIP_ERR_UNSUPPORTED;
    if (!name || !name_len || (capacity && !out)) return XIP_ERR_ARG;
    return hooks.module_manifest(name, name_len, out, capacity);
}

static int h_module_rescan(void *user)
{
    (void)user;
    return hooks.module_rescan ? hooks.module_rescan() : XIP_ERR_UNSUPPORTED;
}

static void restart_later(void *arg) { (void)arg; esp_restart(); }

static int h_reset(void *user)
{
    (void)user;
    esp_timer_handle_t t;
    const esp_timer_create_args_t a = {.callback = restart_later, .name = "restart"};
    if (esp_timer_create(&a, &t) != ESP_OK || esp_timer_start_once(t, 300000) != ESP_OK) return XIP_ERR_NOMEM;
    return XIP_OK;
}

/* ---- the managed heap: a module frees what it took before it stops, and the host checks the balance ---- */
static void *h_mem_alloc(void *user, size_t size)
{
    /* Leave 32 KiB for the host. This checks current heap availability; other
     * firmware allocations can still consume that margin later. */
    const size_t margin = 32768;
    size_t available = heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
    if (available <= margin || size > available - margin) return NULL;
    void *ptr = xip_mem_alloc(&I(user)->mem, size);
    if (ptr && heap_caps_get_free_size(MALLOC_CAP_DEFAULT) < margin) {
        xip_mem_free(&I(user)->mem, ptr);
        return NULL;
    }
    return ptr;
}
static int h_mem_free(void *user, void *ptr) { return xip_mem_free(&I(user)->mem, ptr) == 0 ? XIP_OK : XIP_ERR_ARG; }

/* ---- packets (include/xip.h, PKT): the C pool, and what each module holds ---- */

// TODO: move to xip_pool.c
static SemaphoreHandle_t pool_lock;

static size_t pool_free_heap(void) { return heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT); }

/* The pool is allocated when the first packet is wanted: a device whose modules never use one pays nothing. */
static xip_pool *pool_get(void)
{
    if (!pool_lock) {
        SemaphoreHandle_t m = xSemaphoreCreateMutex();
        portENTER_CRITICAL(&registry_lock);
        if (!pool_lock) { pool_lock = m; m = NULL; }
        portEXIT_CRITICAL(&registry_lock);
        if (m) vSemaphoreDelete(m);
    }
    xSemaphoreTake(pool_lock, portMAX_DELAY);
    if (!pool) {
        if (hooks.pool) {                       /* the application's pool: DMesh's QUIC pool */
            if (hooks.pool->slot_size(hooks.pool->ctx) >= XIP_PKT_SLOT_MIN) pool = xip_pool_external(hooks.pool);
            else ESP_LOGE(TAG, "application pool slots are too small (%u < %u)", (unsigned)hooks.pool->slot_size(hooks.pool->ctx), (unsigned)XIP_PKT_SLOT_MIN);
        } else {
            pool = xip_pool_create(CONFIG_XIP_PKT_SLOTS, XIP_PKT_SLOT_MIN);
            xip_pool_set_policy(pool, CONFIG_XIP_PKT_KEEP, pool_free_heap, CONFIG_XIP_PKT_HEAP_RESERVE);
        }
    }
    xSemaphoreGive(pool_lock);
    return pool;
}

#define PKT_TAIL (XIP_PKT_SLOT_MIN - XIP_PKT_HEADROOM - XIP_PKT_MAX)

static void drop_slot(int slot) { xip_pool_release(pool,slot); }
static xip_packet_ref packet_take(uint32_t packet) {
    xip_packet_ref *p=xip_packet_get(pool,packet);
    return p?*p:(xip_packet_ref){.slot=-1};
}

static int h_pkt_alloc(void *user, uint32_t *out)
{
    (void)user;
    xip_pool *p = pool_get();
    if (!out) return XIP_ERR_ARG;
    *out = 0;
    if (!p) return XIP_ERR_NOMEM;
    int slot = xip_pool_acquire(p, 0);
    if (slot < 0) return XIP_ERR_NOMEM;
    uint32_t h = xip_packet_handle(p,slot,XIP_PKT_HEADROOM,0);
    if (!h) { drop_slot(slot); return XIP_ERR_BUSY; }
    *out = h;
    return XIP_OK;
}

static int h_pkt_buf(void *user, uint32_t pkt, uint8_t **ptr, size_t *max)
{
    (void)user;
    xip_packet_ref *e = xip_packet_get(pool,pkt);
    if (!e || !ptr || !max) return XIP_ERR_ARG;
    if (xip_pool_refs(pool, e->slot) != 1) return XIP_ERR_BUSY;     /* shared: something else still reads it */
    *ptr = xip_pool_slot(pool, e->slot) + e->start;
    *max = xip_pool_slot_size(pool) - e->start - PKT_TAIL;
    if (*max > XIP_PKT_MAX) *max = XIP_PKT_MAX;
    return XIP_OK;
}

static int h_pkt_span(void *user, uint32_t pkt, uint8_t **base, size_t *capacity)
{
    (void)user;
    xip_packet_ref *e = xip_packet_get(pool,pkt);
    if (!e || !base || !capacity || !pool) return XIP_ERR_ARG;
    *base = xip_pool_slot(pool, e->slot);
    *capacity = xip_pool_slot_size(pool);
    return XIP_OK;
}

static int h_pkt_pool_info(void *ctx,xip_packet_pool_info *out)
{
    (void)ctx;xip_pool *p=pool_get();if(!p || !out || out->size<sizeof(*out))return XIP_ERR_ARG;
    *out=(xip_packet_pool_info){.size=sizeof(*out),.slot_bytes=xip_pool_slot_size(p),
        .headroom=XIP_PKT_HEADROOM,.max_packet=XIP_PKT_MAX,.capacity=xip_pool_slots(p),.available=xip_pool_available(p)};
    return XIP_OK;
}
static int h_pkt_acquire(void *ctx,uint32_t reserve,uint32_t *out)
{
    (void)ctx;xip_pool *p=pool_get();if(!p || !out || reserve>UINT16_MAX)return XIP_ERR_ARG;
    *out=0;int slot=xip_pool_acquire(p,(uint16_t)reserve);if(slot<0)return XIP_ERR_NOMEM;
    *out=xip_packet_handle(p,slot,XIP_PKT_HEADROOM,0);return XIP_OK;
}

/* Generic HAL implementations; no upload format or loader policy. */
#include "socket_hal.inc"
#include "flash_hal.inc"

static int h_pkt_keep(void *user, uint32_t pkt)
{
    (void)user;return xip_packet_retain(pool,pkt)==0?XIP_OK:XIP_ERR_ARG;
}

static int h_pkt_free(void *user, uint32_t pkt)
{
    xip_packet_ref e = packet_take(pkt);
    if (e.slot < 0) return XIP_ERR_ARG;
    drop_slot(e.slot);
    return XIP_OK;
}

static int h_pkt_info(void *user, uint32_t *headroom, uint32_t *max)
{
    (void)user;
    if (!headroom || !max) return XIP_ERR_ARG;
    *headroom = XIP_PKT_HEADROOM;
    *max = XIP_PKT_MAX;
    return XIP_OK;
}

/* One global backlog, grown on the owner when bursts need more descriptors.
 * Payload allocation is separately bounded by the common pool's heap reserve. */
static bool packet_deliveries_prepare(void)
{
    portENTER_CRITICAL(&registry_lock);
    size_t capacity=packet_queue.capacity,count=packet_queue.count;
    bool grow=packet_queue.grow,rx_pressure=packet_queue.rx_pressure;
    packet_queue.rx_pressure=false;
    portEXIT_CRITICAL(&registry_lock);
    const int64_t now=esp_timer_get_time();
    const bool recent_burst=now<packet_queue.rx_burst_until;
    if(count>1 || rx_pressure)packet_queue.rx_burst_until=now+250000;
    /* A short burst cache is prepared only on this owner, never in a radio
     * callback. Grow with observed demand, not with the queue's full capacity. */
    unsigned keep=CONFIG_XIP_PKT_KEEP;
    if(now<packet_queue.rx_burst_until){
        if(recent_burst && packet_queue.rx_keep>keep)keep=packet_queue.rx_keep;
        if(rx_pressure && keep<32)keep=keep?keep*2:4;
        if(count+CONFIG_XIP_PKT_KEEP>keep)keep=(unsigned)count+CONFIG_XIP_PKT_KEEP;
        const unsigned maximum=CONFIG_XIP_PKT_KEEP>32?CONFIG_XIP_PKT_KEEP:32;
        if(keep>maximum)keep=maximum;
    }
    if(pool && packet_queue.rx_keep!=keep){
        const unsigned previous=packet_queue.rx_keep;
        xip_pool_set_keep(pool,(uint16_t)keep);
        packet_queue.rx_keep=keep;
        if(keep<previous)xip_pool_trim(pool);
    }
    if(pool && keep>CONFIG_XIP_PKT_KEEP && xip_pool_cached_available(pool)<keep)
        xip_pool_prepare_rx(pool);
    if(capacity && !grow && count<capacity/2)return true;
    if(capacity>SIZE_MAX/2)return true;
    size_t target=capacity?capacity*2:CONFIG_XIP_PKT_QUEUE_INITIAL;
    const size_t free_bytes=heap_caps_get_free_size(MALLOC_CAP_INTERNAL|MALLOC_CAP_8BIT);
    const size_t maximum=free_bytes>CONFIG_XIP_PKT_HEAP_RESERVE?
        (free_bytes-CONFIG_XIP_PKT_HEAP_RESERVE)/sizeof(packet_delivery):0;
    if(target>maximum)target=maximum;
    if(target<=capacity)return capacity!=0;
    const size_t bytes=target*sizeof(packet_delivery);
    packet_delivery *replacement=malloc(bytes);
    if(!replacement)return capacity!=0;
    portENTER_CRITICAL(&registry_lock);
    packet_delivery *old=NULL;
    if(packet_queue.capacity==capacity){
        old=packet_queue_resize(&packet_queue,replacement,target);replacement=NULL;
    }
    portEXIT_CRITICAL(&registry_lock);
    free(old);free(replacement);return true;
}

static int deliver_checked(struct xip_instance *t,uint32_t expected_id,int slot,uint16_t start,uint16_t len,int component,int method,
                   uint32_t from,uint64_t id,int32_t value,uint32_t lflags,uint32_t time_us,const xip_udp_address *src)
{
    bool wake=false;int rc=XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    if(known_locked(t) && (!expected_id || t->id==expected_id)){
        packet_delivery packet={.owner=t->id,.slot=(int16_t)slot,.start=start,.len=len,
            .component=component,.method=method,.from=from,.id=id,.value=value,.lflags=lflags,.time_us=time_us,
            .src.port=src?src->port:0,.src.scope=src?src->scope:0};
        if(src)memcpy(packet.src.ip,&src->ip,16);
        wake=packet_queue.count==0;
        rc=packet_queue_push(&packet_queue,&packet)?XIP_OK:XIP_ERR_BUSY;
    }
    portEXIT_CRITICAL(&registry_lock);
    if(rc==XIP_OK && wake){
        /* A full control queue already guarantees that the worker will run.
         * It checks the packet FIFO before sleeping; a missed wake loses no lease. */
        xip_event_t ev={.kind=XIP_EV_PACKET};
        (void)xQueueSend(worker_queue,&ev,0);
    }
    return rc;
}

static int deliver_unbound(int slot,uint16_t start,uint16_t len,int component,int port,uint32_t from,uint64_t id,int32_t value,uint32_t flags,uint32_t time_us)
{
    packet_delivery packet={.owner=0,.slot=(int16_t)slot,.start=start,.len=len,.component=component,.method=port,
        .from=from,.id=id,.value=value,.lflags=flags,.time_us=time_us};
    portENTER_CRITICAL(&registry_lock);bool wake=packet_queue.count==0;
    int rc=packet_queue_push(&packet_queue,&packet)?0:XIP_ERR_BUSY;portEXIT_CRITICAL(&registry_lock);
    if(!rc && wake){xip_event_t ev={.kind=XIP_EV_PACKET};(void)xQueueSend(worker_queue,&ev,0);}
    return rc;
}
static bool protocol_on_demand(int slot,uint16_t start,uint16_t len,uint32_t from,uint64_t peer,int32_t signal,uint32_t time_us)
{
    if(!hooks.request_start)return false;
    if(deliver_unbound(slot,start,len,-1,XIP_PORT_QUIC_ENDPOINT,from,peer,signal,XIP_CALL_LINK,time_us)<0)drop_slot(slot);
    return true;
}

static int deliver(struct xip_instance *t, int slot, uint16_t start, uint16_t len, int component, int method,
                   uint32_t from, uint64_t id, int32_t value, uint32_t lflags, uint32_t time_us, const xip_udp_address *src)
{
    return deliver_checked(t,0,slot,start,len,component,method,from,id,value,lflags,time_us,src);
}

#include "stream_hal.inc"
static const xip_stream *stream_table(void) { return &module_stream_table; }

/* ---- envelopes: parsed and answered by the runtime, never by a module (include/xip_envelope.h) ---- */
static void env_add(struct xip_instance *in, uint32_t dst, uint64_t token, int32_t component, int32_t method, uint64_t id)
{
    portENTER_CRITICAL(&registry_lock);
    xip_env_add(&in->env, dst, token, component, method, id);
    portEXIT_CRITICAL(&registry_lock);
}
static bool env_find(struct xip_instance *in, uint32_t dst, uint64_t token, int32_t component, int32_t method, xip_env_slot *out)
{
    portENTER_CRITICAL(&registry_lock);
    const bool found = xip_env_find(&in->env, dst, token, component, method, out);
    portEXIT_CRITICAL(&registry_lock);
    return found;
}
static void env_done(struct xip_instance *in, const xip_env_slot *s)
{
    portENTER_CRITICAL(&registry_lock);
    xip_env_done(&in->env, s->dst, s->token, s->component, s->method, s->id);
    portEXIT_CRITICAL(&registry_lock);
}

static int h_send(void *user, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t id)
{
    struct xip_instance *in = I(user);
    if (dst >= XIP_ADDR_LINK) {                                  /* ERR_BUSY keeps the packet */
        /* The answer to a 0x444d0002 datagram goes back as one: the long header (the request's CIDs swapped) is written into the empty
         * headroom in front of the payload, and the packet is sent from there. A retry after BUSY writes the same prefix again. */
        xip_packet_ref *he = xip_packet_get(pool,pkt);
        /* The answer to an envelope request (CALL_ENVELOPE) is wrapped here: {1: component, 2: method, 3: id, 6: the module's map} or, for a bare
         * negative integer, {.., 7: error}. The head goes in front of the payload at the same start (the payload moves up), as the datagram header
         * below does; BUSY takes both off again. */
        const bool more = (method & XIP_SEND_MORE) != 0;           /* another answer to an open envelope request (a subscription's events) */
        method &= ~XIP_SEND_MORE;
        xip_env_slot env = {0};
        size_t env_head = 0;
        const size_t env_len = len;
        if (he && len <= XIP_PKT_MAX && env_find(in, dst, id, component, method, &env)) {
            uint8_t head[40];
            uint8_t *at = xip_pool_slot(pool, he->slot) + he->start;
            int32_t error = 0;
            const bool failed = xip_envelope_is_error(at, len, &error);
            env_head = xip_envelope_head(head, sizeof head, env.component, env.method, env.id, failed ? error : 0);
            if (!env_head || len + env_head > XIP_PKT_MAX || (size_t)he->start + len + env_head > xip_pool_slot_size(pool)) {
                const int slot = he->slot;
                if (!more) env_done(in, &env);
                (void)packet_take(pkt);
                drop_slot(slot);
                in->stats.pkt_dropped++;
                return XIP_ERR_SIZE;
            }
            memmove(at + env_head, at, len);
            memcpy(at, head, env_head);
            len = failed ? env_head : len + env_head;          /* the error head is the whole answer; the old payload stays behind it */
        }
        const uint16_t orig_start = he ? he->start : 0;
        uint16_t pstart = orig_start, plen = (uint16_t)len;
        int wrapped = 0;
        uint64_t rid = 0;                                               /* the answer must carry the request's id (field 3; absent = 0) */
        if (he && len <= XIP_PKT_MAX) {
            int64_t v = 0;
            if (xip_cbor_get_int(xip_pool_slot(pool, he->slot) + he->start, len, 3, &v) == 0 && v >= 0) rid = (uint64_t)v;
            wrapped = quic_reply_prefix(dst, id, rid, he->slot, &pstart, &plen);
        }
        if (wrapped < 0) {
            const int slot = he->slot;
            if (env_head && !more) env_done(in, &env);
            (void)packet_take(pkt);
            drop_slot(slot);
            in->stats.pkt_dropped++;
            return wrapped;
        }
        const size_t olen = len;
        if (wrapped) len = plen;
        const int rc = link_send(in, pkt, len, dst, component, method, id);
        if (wrapped && rc != XIP_ERR_BUSY && !more) quic_reply_finish(dst, id, rid);     /* a subscription's events keep the note (until it expires) */
        if (wrapped && rc == XIP_ERR_BUSY && (he = xip_packet_get(pool,pkt)) != NULL) {      /* the sender keeps the packet and retries: take the header off again */
            uint8_t *at = xip_pool_slot(pool, he->slot) + he->start;
            memmove(at, at + (len - olen), olen);
        }
        if (env_head) {
            if (rc != XIP_ERR_BUSY) { if (!more) env_done(in, &env); }
            else if ((he = xip_packet_get(pool,pkt)) != NULL) {                              /* and the envelope head */
                uint8_t *at = xip_pool_slot(pool, he->slot) + he->start;
                memmove(at, at + env_head, env_len);
            }
        }
        return rc;
    }
    xip_packet_ref e = packet_take(pkt);       /* consumed on success and on failure */
    if (e.slot < 0) return XIP_ERR_ARG;
    int rc;
    if (len > XIP_PKT_MAX || e.start + len > xip_pool_slot_size(pool)) {
        rc = XIP_ERR_SIZE;
    } else if (dst == XIP_ADDR_MAIN && in->in_request && id == XIP_REQUEST_ID) {
        /* the answer to xip_instance_request */
        if (in->responded) rc = XIP_ERR_BUSY;
        else if (len > CONFIG_XIP_RESPONSE_MAX || !in->response) rc = XIP_ERR_SIZE;
        else {
            memcpy(in->response, xip_pool_slot(pool, e.slot) + e.start, len);
            in->response_len = len;
            in->responded = true;
            rc = 0;
        }
        drop_slot(e.slot);
        if (rc == 0) in->stats.pkt_sent++;
        return rc;
    } else if (dst == XIP_ADDR_MAIN) {
        rc = hooks.packet ? hooks.packet(hooks.packet_ctx, in->name, in->component, component, method, id,
                                         xip_pool_slot(pool, e.slot) + e.start, len) : XIP_ERR_NOTFOUND;
        drop_slot(e.slot);                                         /* lent to the hook for the call */
        if (rc == 0) in->stats.pkt_sent++;
        return rc;
    } else if (dst == XIP_ADDR_ENVELOPE) {
        /* A module hands the runtime an envelope to dispatch, the way the 0x444d0002 path does: parsed here, delivered to the module of its component
         * (or the console for a native one) as an ordinary request from this module with `id` as the token; the answer returns as a message. */
        xip_envelope env;
        if (xip_envelope_parse(xip_pool_slot(pool, e.slot) + e.start, len, &env) != XIP_ENV_REQUEST) rc = XIP_ERR_ARG;
        else {
            int activation=0;
            struct xip_instance *t = request_target(env.component,0,&activation);
            if (!t || t == in) rc = t ? XIP_ERR_ARG : activation;
            else {
                rc = deliver(t, e.slot, e.start, (uint16_t)len, t->component, 0, in->component, id, 0, XIP_CALL_LINK | XIP_CALL_ENVELOPE, 0, NULL);
                if (rc == 0) { in->stats.pkt_sent++; return 0; }
            }
        }
    } else {
        struct xip_instance *t = by_component((int)dst, NULL);
        if(!t && component==(int)dst && method>0){int activation=0;t=request_target(component,0,&activation);if(!t){drop_slot(e.slot);return activation;}}
        /* the answer to an envelope request that came from a module (ADDR_ENVELOPE): wrapped as for a datagram, in front of the payload */
        xip_env_slot es;
        if (t && env_find(in, dst, id, component, method & ~XIP_SEND_MORE, &es)) {
            uint8_t head[40];
            uint8_t *at = xip_pool_slot(pool, e.slot) + e.start;
            int32_t error = 0;
            const bool failed = xip_envelope_is_error(at, len, &error);
            const size_t hl = xip_envelope_head(head, sizeof head, es.component, es.method, es.id, failed ? error : 0);
            if (hl && len + hl <= XIP_PKT_MAX && (size_t)e.start + len + hl <= xip_pool_slot_size(pool)) {
                memmove(at + hl, at, len);
                memcpy(at, head, hl);
                len = failed ? hl : len + hl;
                if (!(method & XIP_SEND_MORE)) env_done(in, &es);
            }
            method &= ~XIP_SEND_MORE;
        }
        rc = t ? deliver(t, e.slot, e.start, (uint16_t)len, component, method, in->component, id, 0, 0, 0, NULL) : XIP_ERR_NOTFOUND;
        if (rc == 0) { in->stats.pkt_sent++; return 0; }
    }
    in->stats.pkt_dropped++;
    drop_slot(e.slot);
    return rc;
}

int xip_instance_send(xip_instance_t *in, int component, int method, uint64_t id, const uint8_t *data, size_t len)
{
    xip_pool *p = pool_get();
    if (!in || (len && !data) || !p) return in && p ? XIP_ERR_ARG : XIP_ERR_NOMEM;
    if (len > XIP_PKT_MAX) return XIP_ERR_SIZE;
    int slot = xip_pool_acquire(p, 0);                         /* the application may use every packet */
    if (slot < 0) return XIP_ERR_NOMEM;
    if (len) memcpy(xip_pool_slot(p, slot) + XIP_PKT_HEADROOM, data, len);
    int rc = deliver(in, slot, XIP_PKT_HEADROOM, (uint16_t)len, component, method, XIP_ADDR_MAIN, id, 0, 0, 0, NULL);
    if (rc) { drop_slot(slot); in->stats.pkt_dropped++; }
    return rc;
}

/* ---- links: frames between transports and modules (the `link` component, include/xip.h) ----
 * A link is a name and an address (XIP_ADDR_LINK | index). Its provider is the runtime (xip_link_native_register: a driver) or a
 * module (link.provide); its subscribers are modules (link.open). Frames go from the provider to every subscriber as packets with
 * XIP_CALL_LINK, unless a route (link.route) forwards them first, from the driver's context; `send` to the address goes to the
 * provider. Accepted drivers release their own references; BUSY retains the sender's reference. Tables under the registry lock; delivery outside it. */
#define XIP_LINKS_MAX 12 /* eight built-in providers plus module links */
#define XIP_LINK_SUBS 6
#define XIP_LINK_ROUTES 16
typedef struct {
    bool used;
    char name[16];
    uint8_t provider;                       /* XIP_LINK_NONE / NATIVE / MODULE */
    int owner;                              /* component of the providing module */
    int tx_component, tx_method;            /* where a module provider receives what is sent to the link */
    xip_link_tx_fn tx;                      /* a native provider */
    void *tx_ctx;
    uint32_t mtu, rate_kbps, flags, state;
    uint32_t received, sent, forwarded, dropped;
    uint8_t nsubs;
    struct { int owner, component, method; } subs[XIP_LINK_SUBS];
    uint8_t key_len, key_offset;            /* classify: the connection identifier in a frame (key_len 0: none) */
    uint8_t nroutes;
    struct { uint8_t key[16]; uint32_t egress; uint64_t token; uint32_t flags; } routes[XIP_LINK_ROUTES];
} link_t;
static link_t links[XIP_LINKS_MAX];

static link_t *link_named_locked(const char *name, bool create)
{
    size_t n = name ? strnlen(name, sizeof(links[0].name)) : 0;
    if (!n || n >= sizeof(links[0].name)) return NULL;
    link_t *free_slot = NULL;
    for (int i = 0; i < XIP_LINKS_MAX; i++) {
        if (links[i].used && strcmp(links[i].name, name) == 0) return &links[i];
        if (!links[i].used && !free_slot) free_slot = &links[i];
    }
    if (!create || !free_slot) return NULL;
    memset(free_slot, 0, sizeof(*free_slot));
    free_slot->used = true;
    memcpy(free_slot->name, name, n);
    return free_slot;
}
static link_t *link_at_locked(uint32_t addr)
{
    if (addr < XIP_ADDR_LINK || addr - XIP_ADDR_LINK >= XIP_LINKS_MAX) return NULL;
    link_t *l = &links[addr - XIP_ADDR_LINK];
    return l->used ? l : NULL;
}
static uint32_t link_addr(const link_t *l) { return XIP_ADDR_LINK + (uint32_t)(l - links); }
static uint32_t now_us32(void) { return (uint32_t)esp_timer_get_time(); }

static int c_link_open(void *ctx, const char *name, int component, int method)
{
    struct xip_instance *in = I(ctx);
    int rc = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_named_locked(name, true);
    if (!l) rc = name ? XIP_ERR_NOMEM : XIP_ERR_ARG;
    else {
        unsigned k = 0;
        while (k < l->nsubs && !(l->subs[k].owner == in->component && l->subs[k].component == component && l->subs[k].method == method)) k++;
        if (k == l->nsubs) {
            if (l->nsubs == XIP_LINK_SUBS) rc = XIP_ERR_NOMEM;
            else { l->subs[k].owner = in->component; l->subs[k].component = component; l->subs[k].method = method; l->nsubs++; }
        }
        if (!rc) rc = (int)link_addr(l);
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static unsigned link_receivers_locked(const link_t *l)
{
    unsigned n = 0;
    for (unsigned k = 0; k < l->nsubs; k++) if (l->subs[k].method != XIP_LINK_EGRESS) n++;
    return n;
}

static void subs_remove_locked(link_t *l, int owner)
{
    for (unsigned k = 0; k < l->nsubs;) {
        if (l->subs[k].owner != owner) { k++; continue; }
        l->subs[k] = l->subs[--l->nsubs];
    }
}

static int c_link_close(void *ctx, uint32_t addr)
{
    struct xip_instance *in = I(ctx);
    int rc = XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) {
        rc = XIP_OK;
        subs_remove_locked(l, in->component);
        if (l->provider == XIP_LINK_NONE && !l->nsubs) l->used = false;
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static int c_link_provide(void *ctx, const char *name, int component, int method, uint32_t mtu)
{
    struct xip_instance *in = I(ctx);
    int rc = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_named_locked(name, true);
    if (!l) rc = name ? XIP_ERR_NOMEM : XIP_ERR_ARG;
    else if (l->provider == XIP_LINK_NATIVE || (l->provider == XIP_LINK_MODULE && l->owner != in->component)) rc = XIP_ERR_BUSY;
    else {
        l->provider = XIP_LINK_MODULE; l->owner = in->component;
        l->tx_component = component; l->tx_method = method; l->mtu = mtu; l->state = XIP_LINK_DOWN;
        rc = (int)link_addr(l);
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

/* An event for every subscriber of the link (READY, STATE): queued for the worker, never dropped silently by a full inbox. */
static void link_event(uint32_t addr, uint32_t kind, int32_t value, uint64_t id)
{
    const xip_quic_rx_ops *ops = atomic_load(&quic_rx);
    if (ops && kind == XIP_LINK_EVENT_READY && ops->ready) ops->ready(ops->ctx, (int)addr);
    if (ops && kind == XIP_LINK_EVENT_STATE && ops->state) ops->state(ops->ctx, (int)addr, (uint32_t)value);
    struct { int owner, component, method; } subs[XIP_LINK_SUBS];
    unsigned n = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) { n = l->nsubs; memcpy(subs, l->subs, n * sizeof(subs[0])); }
    portEXIT_CRITICAL(&registry_lock);
    quic_module_event(addr,kind,value,id);
    for (unsigned k = 0; k < n; k++) {
        if (subs[k].method == XIP_LINK_EGRESS) continue;
        struct xip_instance *t = by_component(subs[k].owner, NULL);
        if (!t) continue;
        xip_event_t ev = {.kind = XIP_EV_LINK, .inst = t->id, .component = subs[k].component, .method = subs[k].method, .value = value,
                          .id = id, .from = addr, .flags = XIP_CALL_LINK | XIP_CALL_LINK_EVENT | (kind << 8)};
        (void)xQueueSend(worker_queue, &ev, 0);
    }
}

/* Send the frame in `slot` (one reference is the caller's) to the link: on 0 the runtime owns the reference; otherwise the caller
 * keeps it. `sender`, `component`, `method`: where the packet returns when done (component or method 0 and 0: nowhere). */
static int link_tx(uint32_t dst, int slot, uint16_t start, uint16_t len, uint64_t token, int sender, int component, int method)
{
    xip_link_tx_fn tx = NULL;
    void *tx_ctx = NULL;
    int provider = XIP_LINK_NONE, owner = 0, tx_component = 0, tx_method = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(dst);
    if (l) { provider = l->provider; owner = l->owner; tx_component = l->tx_component; tx_method = l->tx_method; tx = l->tx; tx_ctx = l->tx_ctx; }
    portEXIT_CRITICAL(&registry_lock);
    if (provider == XIP_LINK_NONE) return XIP_ERR_NOTFOUND;
    /* TX may use reserved prefix storage; payload references remain immutable.
     * There is no sender ledger or return-packet completion path. */
    (void)sender;(void)component;(void)method;
    int rc;
    if (provider == XIP_LINK_NATIVE && tx) {
        xip_link_packet pk = {.data = xip_pool_slot(pool, slot) + start, .len = len, .headroom = start, .slot = slot, .pool = pool};
        rc = tx(tx_ctx, token, &pk);
        if (rc < 0) {

        } else {
            if (rc == XIP_LINK_TX_DONE) drop_slot(slot);     /* sent already: the runtime's reference ends here */
            rc = 0;
        }
    } else if (provider == XIP_LINK_MODULE) {
        struct xip_instance *t = by_component(owner, NULL);
        rc = t ? deliver(t, slot, start, len, tx_component, tx_method, sender, token, 0, XIP_CALL_LINK, 0, NULL) : XIP_ERR_NOTFOUND;

        /* Success moves the reference into the provider's inbox. */
    } else {
        rc = XIP_ERR_UNSUPPORTED;

    }
    portENTER_CRITICAL(&registry_lock);
    link_t *l2 = link_at_locked(dst);
    if (l2) { if (rc == 0) l2->sent++; else if (rc != XIP_ERR_BUSY) l2->dropped++; }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

/* `send` to a link address. ERR_BUSY is the one failure that leaves the packet with the sender. */
static int link_send(struct xip_instance *in, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t token)
{
    xip_packet_ref *held = xip_packet_get(pool,pkt);
    if (!held) {
        return XIP_ERR_ARG;
    }
    const int slot = held->slot;
    const uint16_t start = held->start;
    if (len > XIP_PKT_MAX || start + len > xip_pool_slot_size(pool)) {
        (void)packet_take(pkt);
        drop_slot(slot);
        in->stats.pkt_dropped++;
        return XIP_ERR_SIZE;
    }
    /* TX BUSY leaves this same reference with the sender. */
    int rc = link_tx(dst, slot, start, (uint16_t)len, token, in->component, component, method);
    if (rc == XIP_ERR_BUSY) return rc;
    if (rc == 0) { in->stats.pkt_sent++; return 0; }
    drop_slot(slot);
    in->stats.pkt_dropped++;
    return rc;
}

static int h_pkt_tx(void *ctx,uint32_t packet,uint32_t start,uint32_t len,uint32_t link,uint64_t peer)
{
    (void)ctx;xip_packet_ref *p=xip_packet_get(pool,packet);if(!p)return XIP_ERR_ARG;
    int slot=p->slot;
    if(!len || start>xip_pool_slot_size(pool) || len>xip_pool_slot_size(pool)-start){drop_slot(slot);return XIP_ERR_ARG;}
    int rc=link_tx(link,slot,(uint16_t)start,(uint16_t)len,peer,0,0,0);
    if(rc<0 && rc!=XIP_ERR_BUSY)drop_slot(slot);
    return rc;
}

/* The provider's side of completion, readiness and state. */
static int c_link_tx_done(void *ctx, uint32_t addr, uint32_t pkt, int outcome)
{
    struct xip_instance *in = I(ctx);
    bool mine;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    mine = l && l->provider == XIP_LINK_MODULE && l->owner == in->component;
    portEXIT_CRITICAL(&registry_lock);
    if (!mine) return XIP_ERR_ARG;
    xip_packet_ref e = packet_take(pkt);
    if (e.slot < 0) return XIP_ERR_ARG;
    (void)outcome;drop_slot(e.slot);                                      /* move the sole reference back, or release it */
    return XIP_OK;
}

static int c_link_ready(void *ctx, uint32_t addr)
{
    struct xip_instance *in = I(ctx);
    bool mine;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    mine = l && l->provider == XIP_LINK_MODULE && l->owner == in->component;
    portEXIT_CRITICAL(&registry_lock);
    if (!mine) return XIP_ERR_ARG;
    link_event(addr, XIP_LINK_EVENT_READY, 0, 0);
    return XIP_OK;
}

static int link_set_state(uint32_t addr, uint32_t state, uint32_t rate_kbps, int owner)
{
    bool changed = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (!l || (owner >= 0 && !(l->provider == XIP_LINK_MODULE && l->owner == owner))) { portEXIT_CRITICAL(&registry_lock); return XIP_ERR_ARG; }
    changed = l->state != state || l->rate_kbps != rate_kbps;
    l->state = state; l->rate_kbps = rate_kbps;
    portEXIT_CRITICAL(&registry_lock);
    if (changed) link_event(addr, XIP_LINK_EVENT_STATE, (int32_t)state, rate_kbps);
    return XIP_OK;
}
static int c_link_set_state(void *ctx, uint32_t addr, uint32_t state, uint32_t rate_kbps)
{
    return link_set_state(addr, state, rate_kbps, I(ctx)->component);
}

/* ---- receive: the providing side to the subscribers ---- */
/* A frame that matches a route is sent on from here, before any module is woken. True when it was consumed. */
static bool link_route(uint32_t addr, int slot, uint16_t start, uint16_t len)
{
    uint32_t egress = 0, flags = 0;
    uint64_t token = 0;
    bool hit = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l && l->key_len && len >= (unsigned)l->key_offset + l->key_len) {
        const uint8_t *key = xip_pool_slot(pool, slot) + start + l->key_offset;
        for (unsigned k = 0; k < l->nroutes && !hit; k++) {
            if (memcmp(l->routes[k].key, key, l->key_len)) continue;
            egress = l->routes[k].egress; token = l->routes[k].token; flags = l->routes[k].flags;
            hit = true;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if (!hit) return false;
    const bool copy = flags & XIP_ROUTE_COPY;
    int forwarded = slot;
    if (copy) {
        /* COPY is an explicit caller-requested clone for observers plus egress,
         * not a driver's defensive copy of an allegedly owned packet. Each
         * resulting lease has one owner and separate mutable headroom. */
        forwarded = xip_pool_acquire_cached(pool, 0);
        if (forwarded >= 0) memcpy(xip_pool_slot(pool, forwarded) + start, xip_pool_slot(pool, slot) + start, len);
    }
    const int rc = forwarded < 0 ? XIP_ERR_NOMEM : link_tx(egress, forwarded, start, len, token, 0, 0, 0);
    portENTER_CRITICAL(&registry_lock);
    link_t *l2 = link_at_locked(addr);
    if (l2) { if (rc == 0) l2->forwarded++; else l2->dropped++; }
    portEXIT_CRITICAL(&registry_lock);
    if (rc && forwarded >= 0) drop_slot(forwarded);                       /* the forward's lease: nobody took it */
    return !copy;
}

/* The application's single receive function (xip_quic_rx_set): every frame of a link that carries no port is shown to it before the
 * subscribers see it. The runtime keeps the packet; the application copies what it keeps. */
void xip_quic_rx_set(const xip_quic_rx_ops *ops)
{
    atomic_store(&quic_rx, ops);
    if (ops && ops->rx) xip_quic_udp_ensure();      /* the default UDP port's datagrams are QUIC packets for this function */
}
int xip_link_find(const char *name)
{
    int addr = XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_named_locked(name, false);
    if (l) addr = (int)link_addr(l);
    portEXIT_CRITICAL(&registry_lock);
    return addr;
}
bool xip_link_native_up(int addr)
{
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked((uint32_t)addr);
    const bool up = l && l->state == XIP_LINK_UP;
    portEXIT_CRITICAL(&registry_lock);
    return up;
}
static bool quic_rx_offer(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us)
{
    const xip_quic_rx_ops *ops = atomic_load(&quic_rx);
    if ((port && port != XIP_PORT_QUIC) || !len || !ops || !ops->rx) return false;
    char name[sizeof(links[0].name)] = {0};
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) memcpy(name, l->name, sizeof(name));
    const bool stream = l && (l->flags & XIP_LINK_FLAG_STREAM);
    portEXIT_CRITICAL(&registry_lock);
    if (stream && !port) return false;       /* the serial line dispatches its records by port; only the QUIC port is a datagram */
    const xip_rx_info info = {.link = (int)addr, .name = name, .source = source, .signal = signal, .time_us = time_us, .port = port};
    return ops->rx(ops->ctx, &info, xip_pool_slot(pool, slot) + start, len);
}


/* ---- the classifier behind the common QUIC receive function (include/xip_quic.h; the API is documented in xip_runtime.h) ----
 * Tables and counters here; the pure parsing is in xip_quic.c. The mutex is taken on receiving tasks (never in an ISR) for a binary search. */
static xip_quic_table quic_table;
static _Atomic int relay_link=-1;
static xip_quic_replies quic_replies;
static xip_quic_stats quic_stats;
static unsigned quic_short_cid;
static char quic_names[4][16];
static SemaphoreHandle_t quic_mutex;
static StaticSemaphore_t quic_mutex_buf;
#define QSTAT(field) __atomic_fetch_add(&quic_stats.field, 1u, __ATOMIC_RELAXED)

static SemaphoreHandle_t quic_lock(void)
{
    if (!quic_mutex) {
        portENTER_CRITICAL(&registry_lock);
        if (!quic_mutex) quic_mutex = xSemaphoreCreateMutexStatic(&quic_mutex_buf);
        portEXIT_CRITICAL(&registry_lock);
    }
    xSemaphoreTake(quic_mutex, portMAX_DELAY);
    return quic_mutex;
}
static uint32_t quic_now_ms(void) { return (uint32_t)(esp_timer_get_time() / 1000); }

void xip_quic_local_set(const xip_quic_local_ops *ops)
{
    atomic_store(&quic_local, ops);
    if (ops && ops->rx) xip_quic_udp_ensure();
}
void xip_quic_classifier_enable(bool on)
{
    atomic_store(&quic_classifier, on);
    if (on) xip_quic_udp_ensure();
}
void xip_quic_short_cid_len(unsigned len) { quic_short_cid = len; }
int xip_quic_link_enable(const char *name)
{
    if (!name || !*name || strlen(name) >= sizeof(quic_names[0])) return XIP_ERR_ARG;
    int rc = XIP_ERR_NOMEM;
    portENTER_CRITICAL(&registry_lock);
    for (unsigned i = 0; i < 4 && rc; i++) {
        if (!strcmp(quic_names[i], name)) rc = XIP_OK;
        else if (!quic_names[i][0]) { strcpy(quic_names[i], name); rc = XIP_OK; }
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}
int xip_quic_table_add_dcid(const uint8_t *cid, size_t len, unsigned kind, int egress_link, uint64_t token, const uint8_t *out_cid, size_t out_len)
{
    SemaphoreHandle_t m = quic_lock();
    const int rc = xip_quic_table_add(&quic_table, cid, (unsigned)len, kind, (uint32_t)egress_link, token, out_cid, (unsigned)out_len);
    xSemaphoreGive(m);
    return rc;
}
int xip_quic_table_alloc_dcid(unsigned kind, int egress_link, uint64_t token, const uint8_t *out_cid, size_t out_len, uint8_t *cid, size_t *len)
{
    unsigned n = 0;
    SemaphoreHandle_t m = quic_lock();
    const int rc = xip_quic_table_alloc(&quic_table, kind, (uint32_t)egress_link, token, out_cid, (unsigned)out_len, cid, &n);
    if (len) *len = n;
    xSemaphoreGive(m);
    return rc;
}
int xip_quic_table_remove_dcid(const uint8_t *cid, size_t len)
{
    SemaphoreHandle_t m = quic_lock();
    const int rc = xip_quic_table_remove(&quic_table, cid, (unsigned)len);
    xSemaphoreGive(m);
    return rc;
}
unsigned xip_quic_table_count(void)
{
    SemaphoreHandle_t m = quic_lock();
    const unsigned n = quic_table.n;
    xSemaphoreGive(m);
    return n;
}
void xip_quic_stats_get(xip_quic_stats *out)
{
    if (!out) return;
    const uint32_t *src = (const uint32_t *)&quic_stats;
    uint32_t *dst = (uint32_t *)out;
    for (size_t i = 0; i < sizeof(*out) / sizeof(uint32_t); i++) dst[i] = __atomic_load_n(&src[i], __ATOMIC_RELAXED);
}

const uint8_t *xip_quic_pkt_data(const xip_quic_pkt *p, size_t *len)
{
    if (!p || p->slot < 0 || !pool) return NULL;
    if (len) *len = p->len;
    return xip_pool_slot(pool, p->slot) + p->start;
}
void xip_quic_pkt_release(xip_quic_pkt *p)
{
    if (!p || p->slot < 0) return;
    drop_slot(p->slot);
    p->slot = -1;
}
int xip_quic_pkt_send(xip_quic_pkt *p, int link, uint64_t token)
{
    if (!p || p->slot < 0 || link < 0) return XIP_ERR_ARG;
    const int rc = link_tx((uint32_t)link, p->slot, p->start, p->len, token, 0, 0, 0);
    if (rc == 0) { p->slot = -1; return 0; }
    if (rc == XIP_ERR_BUSY) return rc;                       /* the lease stays with the caller */
    drop_slot(p->slot);
    p->slot = -1;
    return rc;
}

/* The answer to a CBOR datagram: prefix the long header (version 0x444d0002, the request's CIDs swapped) in the empty headroom. 0: no note for
 * (link, token), the packet goes as it is; 1: prefixed; negative: it does not fit. */
static int quic_reply_prefix(uint32_t dst, uint64_t token, uint64_t id, int slot, uint16_t *start, uint16_t *len)
{
    if (!quic_mutex) return 0;
    xip_quic_reply r = {0};
    SemaphoreHandle_t m = quic_lock();
    const xip_quic_reply *f = xip_quic_reply_find(&quic_replies, dst, token, id, quic_now_ms());
    if (f) r = *f;
    xSemaphoreGive(m);
    if (!f) return 0;
    uint8_t hdr[1 + 4 + 1 + XIP_QUIC_CID_MAX + 1 + XIP_QUIC_CID_MAX];
    const size_t hl = xip_quic_long_build(hdr, sizeof hdr, XIP_QUIC_VERSION_CBOR, r.dcid, r.dcid_len, r.scid, r.scid_len);
    /* The header goes in FRONT of the payload at the same start: the links need the packet where they expect it (ESP-NOW wants the 144-byte
     * headroom and its alignment for its request), so the payload moves up by the header length. Best effort: a copy of at most one packet. */
    if (!hl || (size_t)*len + hl > XIP_PKT_MAX || (size_t)*start + *len + hl > xip_pool_slot_size(pool)) { quic_reply_finish(dst, token, id); return XIP_ERR_SIZE; }
    uint8_t *at = xip_pool_slot(pool, slot) + *start;
    memmove(at + hl, at, *len);
    memcpy(at, hdr, hl);
    *len = (uint16_t)(*len + hl);
    return 1;
}
static void quic_reply_finish(uint32_t dst, uint64_t token, uint64_t id)
{
    SemaphoreHandle_t m = quic_lock();
    const xip_quic_reply *f = xip_quic_reply_find(&quic_replies, dst, token, id, quic_now_ms());
    if (f) xip_quic_reply_done(&quic_replies, f);
    xSemaphoreGive(m);
}

static bool quic_link_is_quic(const char *name)
{
    /* "espnow": the NAN carrier (the NSY1 magic after the vendor IE) never reaches the link, it goes to the NAN port first */
    if (!strcmp(name, "relay") || !strcmp(name, "ble") || !strcmp(name, "udp") || !strcmp(name, "espnow")) return true;
    for (unsigned i = 0; i < 4; i++) if (quic_names[i][0] && !strcmp(quic_names[i], name)) return true;
    return false;
}

#include "xip_quic_module.inc"

/* True when the packet has gone to its one handler (or was dropped and counted): the caller must not touch `slot` any more. False: declined. */
static bool quic_common_rx(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us, const xip_udp_address *src)
{
    if (!len || (port && port != XIP_PORT_QUIC) || !(atomic_load(&quic_classifier) || atomic_load(&quic_module_owner)) || !pool) return false;
    char name[sizeof(links[0].name)] = {0};
    bool found = false, stream = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) { memcpy(name, l->name, sizeof(name)); stream = l->flags & XIP_LINK_FLAG_STREAM; found = true; }
    portEXIT_CRITICAL(&registry_lock);
    if (!found || (stream && !port) || !(port == XIP_PORT_QUIC || quic_link_is_quic(name))) return false;
    const uint8_t *data = xip_pool_slot(pool, slot) + start;
    xip_quic_hdr h;
    const xip_quic_class cls = xip_quic_classify(data, len, quic_short_cid, &h);
    QSTAT(rx);
    if (cls == XIP_QUIC_NOT_QUIC) { QSTAT(declined); return false; }
    if (cls == XIP_QUIC_MALFORMED) { QSTAT(malformed); drop_slot(slot); return true; }
    if (len > XIP_QUIC_MAX_PACKET) { QSTAT(oversize); drop_slot(slot); return true; }
    const xip_rx_info info = {.link = (int)addr, .name = name, .source = source, .signal = signal, .time_us = time_us, .port = port};
    const xip_quic_local_ops *local = atomic_load(&quic_local);
    xip_quic_pkt pkt = {.slot = slot, .start = start, .len = len};
    if (!h.is_long) {
        xip_quic_entry ent = {0};
        bool hit = false;
        SemaphoreHandle_t m = quic_lock();
        hit = xip_quic_relay_select_at(&quic_table, h.dcid, h.dcid_len, (uint64_t)esp_timer_get_time(), &ent) == 0;
        xSemaphoreGive(m);
        if (!hit) { if (quic_module_offer(&info,slot,start,len)) return true; QSTAT(short_unknown); drop_slot(slot); return true; }
        if (hit && ent.kind == 3) {
            /* The outer circuit header has one packet-number byte. Its body is
             * an opaque complete endpoint packet, including long handshakes. */
            const size_t prefix=1+h.dcid_len+1;
            if(addr==(uint32_t)relay_link || (h.first&3) || len<=prefix || relay_link<0) {drop_slot(slot);return true;}
            uint64_t peer=0;memcpy(&peer,h.dcid,h.dcid_len);
            if(!quic_common_rx((uint32_t)relay_link,XIP_PORT_QUIC,slot,(uint16_t)(start+prefix),
                              (uint16_t)(len-prefix),peer,signal,time_us,NULL))drop_slot(slot);
            return true;
        }
        if (ent.kind == XIP_QUIC_DCID_RELAY) {
            /* forward or drop, no queue: the DCID is replaced in the empty headroom (as quic-lite's rewrite_dcid_in_place), the same
             * pool packet goes to the egress link and the peer address stored in the entry. TODO: long-header packets, per-circuit counters. */
            int shift = 0;
            int rc = xip_quic_short_rewrite(xip_pool_slot(pool, slot) + start, len, &h, ent.out_cid, ent.out_len, start, &shift);
            if (!rc && ent.out_address_len) rc=xip_quic_udp_destination(ent.out_address,&ent.token);
            if (!rc) rc = link_tx(ent.egress, slot, (uint16_t)(start + shift), (uint16_t)(len - shift), ent.token, 0, 0, 0);
            if (rc) { QSTAT(short_relay_failed); drop_slot(slot); return true; }
            QSTAT(short_relay);
            return true;
        }
        if (quic_module_offer(&info,slot,start,len)) return true;
        if (!local || !local->rx) { QSTAT(short_no_handler); drop_slot(slot); return true; }
        QSTAT(short_local);
        local->rx(local->ctx, &info, &h, &pkt);                  /* ownership passes: it releases or passes the packet on */
        return true;
    }
    if (h.version != XIP_QUIC_VERSION_CBOR) {
        if (quic_module_offer(&info,slot,start,len)) return true;
        if (!local || !local->rx) { QSTAT(long_other_dropped); drop_slot(slot); return true; }
        QSTAT(long_other);
        local->rx(local->ctx, &info, &h, &pkt);
        return true;
    }
    /* a CBOR datagram: the envelope follows the SCID; routed like an envelope that arrives on a module's port */
    xip_quic_env env;
    const uint16_t elen = (uint16_t)(len - h.body);
    if (!elen || xip_quic_envelope(data + h.body, elen, &env)) { QSTAT(long_cbor_bad); drop_slot(slot); return true; }
    if (env.response) {                                          /* an answer (6 or 7), e.g. to a request this device sent: never a request */
        if (!local || !local->rx) { QSTAT(long_cbor_response); drop_slot(slot); return true; }
        (void)c_power_received(NULL,data+h.body,elen,1);
        QSTAT(long_cbor_response);
        local->rx(local->ctx, &info, &h, &pkt);
        return true;
    }

    SemaphoreHandle_t m = quic_lock();
    xip_quic_reply_note(&quic_replies, addr, source, env.has_id ? env.id : 0, &h, quic_now_ms());
    xSemaphoreGive(m);
    if (deliver_unbound(slot, (uint16_t)(start + h.body), elen, env.component, 0, addr, source, signal, XIP_CALL_LINK | XIP_CALL_ENVELOPE, time_us)) {
        QSTAT(long_cbor_no_target);                               /* its inbox is full */
        quic_reply_finish(addr, source, env.has_id ? env.id : 0);
        drop_slot(slot);
        return true;
    }
    QSTAT(long_cbor);
    return true;
}

/* The packet in `slot` (one reference is the caller's) goes to the route or to every subscriber: one reference each. */
static void link_fanout(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us, const xip_udp_address *src)
{
    struct { int owner, component, method; } subs[XIP_LINK_SUBS];
    unsigned n = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) l->received++;
    portEXIT_CRITICAL(&registry_lock);
    /* ownership, not fan-out: the QUIC receive function takes the frame (and the packet goes back to the pool here), or routing goes on */
    if (quic_common_rx(addr, port==XIP_PORT_QUIC_ENDPOINT?XIP_PORT_QUIC:port, slot, start, len, source, signal, time_us, src)) return;
    if(port && !port_bound(port) && hooks.request_start){if(deliver_unbound(slot,start,len,0,(int)port,addr,source,signal,XIP_CALL_LINK,time_us)<0)drop_slot(slot);return;}      /* the packet went to its one handler */
    if (quic_rx_offer(addr, port, slot, start, len, source, signal, time_us)) { drop_slot(slot); return; }
    if (!port && link_route(addr, slot, start, len)) return;
    portENTER_CRITICAL(&registry_lock);
    l = link_at_locked(addr);
    if (port) {            /* the module bound to the port, whichever link the frame came on */
        for (int i = 0; i < XIP_PORTS_MAX; i++)
            if (ports[i].port == port && n < XIP_LINK_SUBS) { subs[n].owner = subs[n].component = ports[i].component; subs[n].method = 0; n++; }
    } else if (l) {
        for (unsigned k = 0; k < l->nsubs; k++) {
            if (l->subs[k].method == XIP_LINK_EGRESS) continue;
            memcpy(&subs[n++], &l->subs[k], sizeof(subs[0]));
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    unsigned lost = 0;
    if (!n) { drop_slot(slot); return; }
    for (unsigned k = 0; k < n; k++) {
        if (k + 1 < n) xip_pool_retain(pool, slot);          /* the last subscriber takes the caller's reference */
        struct xip_instance *t = by_component(subs[k].owner, NULL);
        if (!t || deliver(t, slot, start, len, subs[k].component, subs[k].method, addr, source, signal, XIP_CALL_LINK, time_us, src)) {
            drop_slot(slot);
            lost++;
        }
    }
    if (lost) {
        portENTER_CRITICAL(&registry_lock);
        l = link_at_locked(addr);
        if (l) l->dropped += lost;
        portEXIT_CRITICAL(&registry_lock);
    }
}

static int c_link_rx(void *ctx, uint32_t addr, uint32_t pkt, size_t len, uint64_t source, int32_t signal, uint32_t time_us)
{
    struct xip_instance *in = I(ctx);
    xip_packet_ref e = packet_take(pkt);            /* consumed on success and on failure */
    if (e.slot < 0) return XIP_ERR_ARG;
    bool mine;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    mine = l && l->provider == XIP_LINK_MODULE && l->owner == in->component;
    portEXIT_CRITICAL(&registry_lock);
    if (!mine) { drop_slot(e.slot); return XIP_ERR_ARG; }
    if (len > XIP_PKT_MAX || e.start + len > xip_pool_slot_size(pool)) { drop_slot(e.slot); return XIP_ERR_SIZE; }
    in->stats.pkt_sent++;
    link_fanout(addr, 0, e.slot, e.start, (uint16_t)len, source, signal, time_us ? time_us : now_us32(), NULL);
    return XIP_OK;
}

static int c_link_info(void *ctx, uint32_t addr, xip_link_info *out)
{
    (void)ctx;
    if (!out || out->size < sizeof(*out)) return XIP_ERR_ARG;
    int rc = XIP_ERR_NOTFOUND;
    uint32_t size = out->size;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) {
        memset(out, 0, sizeof(*out)); out->size = size;
        out->provider = l->provider; out->state = l->state; out->mtu = l->mtu; out->rate_kbps = l->rate_kbps; out->flags = l->flags;
        out->subscribers = link_receivers_locked(l); out->received = l->received; out->sent = l->sent; out->forwarded = l->forwarded; out->dropped = l->dropped;
        memcpy(out->name, l->name, sizeof(out->name));
        rc = XIP_OK;
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static int c_link_classify(void *ctx, uint32_t addr, uint32_t offset, uint32_t len)
{
    (void)ctx;
    if (!len || len > 16 || offset > 255) return XIP_ERR_ARG;
    int rc = XIP_OK;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (!l) rc = XIP_ERR_NOTFOUND;
    else if (l->key_len != len || l->key_offset != offset) { l->key_len = (uint8_t)len; l->key_offset = (uint8_t)offset; l->nroutes = 0; }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static int c_link_route(void *ctx, uint32_t addr, const uint8_t *key, size_t key_len, uint32_t egress, uint64_t token, uint32_t flags)
{
    (void)ctx;
    int rc = XIP_OK;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (!l) rc = XIP_ERR_NOTFOUND;
    else if (!key || !l->key_len || key_len != l->key_len || !link_at_locked(egress) || egress == addr) rc = XIP_ERR_ARG;
    else {
        unsigned k = 0;
        while (k < l->nroutes && memcmp(l->routes[k].key, key, key_len)) k++;
        if (k == l->nroutes && l->nroutes == XIP_LINK_ROUTES) rc = XIP_ERR_NOMEM;
        else {
            memcpy(l->routes[k].key, key, key_len);
            l->routes[k].egress = egress; l->routes[k].token = token; l->routes[k].flags = flags;
            if (k == l->nroutes) l->nroutes++;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static int c_link_unroute(void *ctx, uint32_t addr, const uint8_t *key, size_t key_len)
{
    (void)ctx;
    int rc = XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l && key && key_len == l->key_len) {
        for (unsigned k = 0; k < l->nroutes; k++) {
            if (memcmp(l->routes[k].key, key, key_len)) continue;
            l->routes[k] = l->routes[--l->nroutes];
            rc = XIP_OK;
            break;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

static const xip_link link_component = {
    .version = XIP_LINK_VERSION, .size = sizeof(xip_link), .open = c_link_open, .close = c_link_close, .provide = c_link_provide,
    .rx = c_link_rx, .tx_done = c_link_tx_done, .ready = c_link_ready, .set_state = c_link_set_state, .info = c_link_info,
    .classify = c_link_classify, .route = c_link_route, .unroute = c_link_unroute,
};
static const xip_link *link_table(void) { return &link_component; }

/* A module ended: its subscriptions go, its links lose their provider (the name stays for the subscribers), and the frames it
 * was holding for the sender are reported as failed. */
static void links_release(struct xip_instance *in)
{
    uint32_t down[XIP_LINKS_MAX];
    unsigned ndown = 0;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_LINKS_MAX; i++) {
        link_t *l = &links[i];
        if (!l->used) continue;
        subs_remove_locked(l, in->component);
        if (l->provider == XIP_LINK_MODULE && l->owner == in->component) {
            l->provider = XIP_LINK_NONE; l->owner = 0; l->state = XIP_LINK_DOWN; l->nroutes = 0;
            down[ndown++] = link_addr(l);
        }
        if (l->provider == XIP_LINK_NONE && !l->nsubs) l->used = false;
    }
    portEXIT_CRITICAL(&registry_lock);
    /* Providers close their holders in HOST_STOP; queued inbox entries are drained separately. */
    for (unsigned k = 0; k < ndown; k++) link_event(down[k], XIP_LINK_EVENT_STATE, XIP_LINK_DOWN, 0);
}

/* The runtime's own transports: a driver registers a link and hands over what it receives. */
int xip_link_native_register(const char *name, uint32_t mtu, uint32_t flags, xip_link_tx_fn tx, void *ctx)
{
    int rc;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_named_locked(name, true);
    if (!l) rc = name ? XIP_ERR_NOMEM : XIP_ERR_ARG;
    else if (l->provider == XIP_LINK_MODULE || (l->provider == XIP_LINK_NATIVE && l->tx != tx)) rc = XIP_ERR_BUSY;
    else { l->provider = XIP_LINK_NATIVE; l->tx = tx; l->tx_ctx = ctx; l->mtu = mtu; l->flags = flags; rc = (int)link_addr(l); }
    portEXIT_CRITICAL(&registry_lock);
    return rc;
}

void xip_link_native_unregister(int addr)
{
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked((uint32_t)addr);
    if (l && l->provider == XIP_LINK_NATIVE) { l->provider = XIP_LINK_NONE; l->tx = NULL; l->state = XIP_LINK_DOWN; l->nroutes = 0; if (!l->nsubs) l->used = false; }
    portEXIT_CRITICAL(&registry_lock);
}

int xip_link_native_set_state(int addr, uint32_t state, uint32_t rate_kbps) { return link_set_state((uint32_t)addr, state, rate_kbps, -1); }
void xip_link_native_ready(int addr) { link_event((uint32_t)addr, XIP_LINK_EVENT_READY, 0, 0); }
void xip_link_native_tx_done(int addr, int slot, int outcome) { (void)addr; (void)outcome; drop_slot(slot); }

static void packet_rx_storage_shortage(void)
{
    portENTER_CRITICAL(&registry_lock);
    bool wake=!packet_queue.rx_pressure;
    packet_queue.rx_pressure=true;
    PACKET_COUNT(&packet_queue,rx_pool_empty);
    portEXIT_CRITICAL(&registry_lock);
    if(wake && worker_queue){
        xip_event_t ev={.kind=XIP_EV_PACKET};
        (void)xQueueSend(worker_queue,&ev,0);
    }
}

/* Receive without a copy: the driver parses into a pool packet, the way DMesh's receive_now_with does. begin returns 0 and a
 * packet to fill (capacity XIP_PKT_MAX at the payload window), 1 when nobody listens (skip the frame), or an error (XIP_ERR_NOMEM:
 * the pool is empty, counted). Then commit (the frame is `len` bytes; stamped now) or abort. */
static int native_rx_begin(int addr, xip_link_rx_buf *buf, bool cached)
{
    xip_pool *p = cached ? pool : pool_get();
    unsigned subs = 0;
    bool routed = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked((uint32_t)addr);
    if (l) { subs = link_receivers_locked(l); routed = l->nroutes != 0; }
    portEXIT_CRITICAL(&registry_lock);
    if (!l || !buf || !p) return XIP_ERR_ARG;
    if (!subs && !routed && !quic_active()) return 1;
    int slot = cached ? xip_pool_acquire_cached(p, 0) : xip_pool_acquire(p, 0);
    if (slot < 0) {
        packet_rx_storage_shortage();
        portENTER_CRITICAL(&registry_lock);
        l = link_at_locked((uint32_t)addr);
        if (l) l->dropped++;
        portEXIT_CRITICAL(&registry_lock);
        return XIP_ERR_NOMEM;
    }
    buf->slot = slot;
    buf->data = xip_pool_slot(p, slot) + XIP_PKT_HEADROOM;
    buf->capacity = XIP_PKT_MAX;
    return XIP_OK;
}

void xip_link_native_rx_prepare(void)
{
    xip_pool *p = pool_get();
    if (p) xip_pool_prepare_rx(p);
}
int xip_link_native_rx_begin(int addr, xip_link_rx_buf *buf) { return native_rx_begin(addr, buf, false); }
int xip_link_native_rx_begin_cached(int addr, xip_link_rx_buf *buf) { return native_rx_begin(addr, buf, true); }

void xip_link_native_rx_commit(int addr, const xip_link_rx_buf *buf, size_t len, uint64_t source, int32_t signal)
{
    if (len > XIP_PKT_MAX) { drop_slot(buf->slot); return; }
    link_fanout((uint32_t)addr, 0, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, source, signal, now_us32(), NULL);
}

void xip_link_native_rx_abort(const xip_link_rx_buf *buf) { drop_slot(buf->slot); }

int xip_link_native_rx(int addr, uint64_t source, int32_t signal, const uint8_t *data, size_t len)
{
    const uint32_t time_us = now_us32();                                 /* the driver's callback, not the delivery */
    xip_pool *p = pool_get();
    unsigned subs = 0;
    bool routed = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked((uint32_t)addr);
    if (l) { subs = link_receivers_locked(l); routed = l->nroutes != 0; }
    portEXIT_CRITICAL(&registry_lock);
    if (!l || (len && !data) || !p) return XIP_ERR_ARG;
    if (!subs && !routed && !quic_active()) return XIP_OK;        /* nobody listens: not worth a packet */
    if (len > XIP_PKT_MAX) return XIP_ERR_SIZE;
    int slot = xip_pool_acquire(p, 0);
    if (slot < 0) {
        packet_rx_storage_shortage();
        portENTER_CRITICAL(&registry_lock);
        l = link_at_locked((uint32_t)addr);
        if (l) l->dropped++;
        portEXIT_CRITICAL(&registry_lock);
        return XIP_ERR_NOMEM;
    }
    if (len) memcpy(xip_pool_slot(p, slot) + XIP_PKT_HEADROOM, data, len);
    link_fanout((uint32_t)addr, 0, slot, XIP_PKT_HEADROOM, (uint16_t)len, source, signal, time_us, NULL);
    return XIP_OK;
}

/* The application sends one frame on a native link: copied into a pool packet (payload window after the headroom). 0: the link has it (the
 * runtime owns the packet); XIP_ERR_BUSY: the link refused it, nothing is kept, retry after the READY callback; other negatives: dropped. */
int xip_link_native_send(int addr, uint64_t dest, const uint8_t *data, size_t len)
{
    xip_pool *p = pool_get();
    if (!p || !data || !len || len > XIP_PKT_MAX) return XIP_ERR_ARG;
    const int slot = xip_pool_acquire(p, 0);
    if (slot < 0) return XIP_ERR_NOMEM;
    memcpy(xip_pool_slot(p, slot) + XIP_PKT_HEADROOM, data, len);
    const int rc = link_tx((uint32_t)addr, slot, XIP_PKT_HEADROOM, (uint16_t)len, dest, 0, 0, 0);
    if (rc) drop_slot(slot);
    return rc;
}

/* Drain runtime-owned deliveries to the ended instance. Module holders are its responsibility. */
static void packets_release(struct xip_instance *in)
{
    if(!pool)return;
    for(;;){
        packet_delivery packet;
        portENTER_CRITICAL(&registry_lock);
        bool removed=packet_queue_remove_owner(&packet_queue,in->id,&packet);
        portEXIT_CRITICAL(&registry_lock);
        if(!removed)break;
        drop_slot(packet.slot);in->stats.pkt_dropped++;
    }
}

int xip_packet_queue_info(uint8_t *out,size_t cap)
{
    size_t depth,capacity;
#if CONFIG_XIP_PACKET_STATS
    packet_queue_counters counters;
#endif
    portENTER_CRITICAL(&registry_lock);
    depth=packet_queue.count;capacity=packet_queue.capacity;
#if CONFIG_XIP_PACKET_STATS
    counters=packet_queue.counters;
#endif
    portEXIT_CRITICAL(&registry_lock);
    xip_cbor_w w;xip_cbor_init(&w,out,cap);
    xip_cbor_map(&w,13);
    xip_cbor_uint(&w,1);xip_cbor_uint(&w,depth);
    xip_cbor_uint(&w,2);xip_cbor_uint(&w,capacity);
#if CONFIG_XIP_PACKET_STATS
    const uint32_t values[]={counters.high_water,counters.accepted,counters.dispatched,counters.queue_full,counters.owner_stopped,counters.rx_pool_empty};
#else
    const uint32_t values[]={0,0,0,0,0,0};
#endif
    for(unsigned k=0;k<6;k++){xip_cbor_uint(&w,3+k);xip_cbor_uint(&w,values[k]);}
    xip_cbor_uint(&w,9);xip_cbor_uint(&w,pool?xip_pool_slots(pool):0);
    xip_cbor_uint(&w,10);xip_cbor_uint(&w,pool?xip_pool_allocated(pool):0);
    xip_cbor_uint(&w,11);xip_cbor_uint(&w,pool?xip_pool_available(pool):0);
    xip_cbor_uint(&w,12);
#if CONFIG_XIP_PACKET_STATS
    xip_cbor_bool(&w,true);
#else
    xip_cbor_bool(&w,false);
#endif
    xip_cbor_uint(&w,13);xip_cbor_uint(&w,pool?xip_pool_cached_available(pool):0);
    return xip_cbor_done(&w);
}

/* ---- instances ---- */
static struct xip_instance *find_locked(uint32_t id)
{
    for (int i = 0; i < XIP_MAX_INSTANCES; i++) if (registry[i] && registry[i]->id == id) return registry[i];
    return NULL;
}

static struct xip_instance *find(uint32_t id)
{
    portENTER_CRITICAL(&registry_lock);
    struct xip_instance *in = find_locked(id);
    portEXIT_CRITICAL(&registry_lock);
    return in;
}

static bool known_locked(const struct xip_instance *in)
{
    for (int i = 0; i < XIP_MAX_INSTANCES; i++) if (in && registry[i] == in) return true;
    return false;
}

/* End an instance: on the worker only. */
static void finish(struct xip_instance *in, int result)
{
    if (exec_lock) xSemaphoreTakeRecursive(exec_lock, portMAX_DELAY);        /* no table call is inside the module while it is released */
    portENTER_CRITICAL(&registry_lock);
    registry[in->slot] = NULL;
    SemaphoreHandle_t waiter = in->waiter_sem;
    in->waiter_sem = NULL;
    if (waiter) *in->waiter_rc = XIP_ERR_NOTFOUND;
    portEXIT_CRITICAL(&registry_lock);
    if (waiter) xSemaphoreGive(waiter);
    for (int i = 0; i < XIP_ALARMS; i++) {
        if (!in->alarms[i].timer) continue;
        esp_timer_stop(in->alarms[i].timer);
        esp_timer_delete(in->alarms[i].timer);
        alarm_ctx[in->slot * XIP_ALARMS + i].seq++;   /* a callback already in flight is stale */
    }
    streams_release(in->id);
    quic_module_release(in);
    if (quic_mutex) { SemaphoreHandle_t m=quic_lock(); xip_quic_relay_release(&quic_table,in->id); xSemaphoreGive(m); relay_link_gc(); }
    xip_power_cancel(in->id);
    xip_info_release(in->id);
    socket_release(in->id);
    flash_release(in->id);
    serial_release(in);
    links_release(in);
    packets_release(in);
    const uint32_t leaked_bytes = xip_mem_used(&in->mem);        /* cannot be freed: nobody knows where it is */
    in->stats.mem_leaked = leaked_bytes;
    if (leaked_bytes) {
        ESP_LOGW(TAG,"%s ended holding %u bytes of managed memory: it needs a reboot to be replaced",in->name,(unsigned)leaked_bytes);
        if (result >= 0) result = XIP_ERR_LEAK;
    }
    xip_periph_release(in);
    xip_done_fn done = in->done;
    void *ctx = in->done_ctx;
    xip_stats_t stats = in->stats;
    ESP_LOGI(TAG, "%s ended: %d (%u calls, max %u us)", in->name, result, (unsigned)stats.calls, (unsigned)stats.max_handler_us);
    free(in->state); free(in->inbox); free(in->start_request); free(in->response);
    free(in);
    if (done) done(ctx, result, &stats);
    if (exec_lock) xSemaphoreGiveRecursive(exec_lock);
}

static int dispatch_x(struct xip_instance *in, const xip_call *call)
{
    const int64_t t0 = esp_timer_get_time();
    if (exec_lock) xSemaphoreTakeRecursive(exec_lock, portMAX_DELAY);       /* a table call from another task waits for the handler */
    int rc = in->handler(&in->table, in->state, call);
    if (exec_lock) xSemaphoreGiveRecursive(exec_lock);
    uint32_t us = (uint32_t)(esp_timer_get_time() - t0);
    in->stats.calls++;
    if (us > in->stats.max_handler_us) in->stats.max_handler_us = us;
    if (us / 1000 > CONFIG_XIP_HANDLER_MS) {
        in->stats.overruns++;
        ESP_LOGW(TAG, "%s: handler %d.%d took %u ms (budget %d)", in->name, call->component, call->method, (unsigned)(us / 1000),
                 CONFIG_XIP_HANDLER_MS);
    }
    return rc;
}

static int dispatch(struct xip_instance *in, int component, int method, int value, uint32_t flags,
                    const uint8_t *data, size_t len)
{
    const xip_call call = {.size = sizeof(call), .component = component, .method = method, .value = value,
                              .flags = flags, .data = data, .len = len};
    return dispatch_x(in, &call);
}

static int dispatch_packet(struct xip_instance *in, int component, int method, const uint8_t *data, size_t len, uint32_t pkt,
                           uint32_t from, uint64_t id, uint32_t flags, int32_t value, uint32_t time_us, const xip_udp_address *src)
{
    xip_udp_address src0 = {0};
    if (src) src0 = *src;
    xip_call call = {.size = sizeof(call), .component = component, .method = method, .value = value, .data = data, .len = len,
                              .pkt = pkt, .from = from, .id = id, .flags = flags, .time_us = time_us,
                              .request_id = in->req_id, .request_has_id = in->req_has_id};
    memcpy(call.src_ip, src0.ip, 16);
    call.src_port = src0.port; call.src_scope = src0.scope;
    return dispatch_x(in, &call);
    return dispatch_x(in, &call);
}

/* After a handler call: a negative result or DONE ends the instance. True when it ended. */
static bool settle(struct xip_instance *in, int rc)
{
    if (rc < 0 || rc == XIP_RET_DONE) {
        finish(in, rc);
        return true;
    }
    return false;
}

static void handle_alarm(struct xip_instance *in, const xip_event_t *ev)
{
    xip_alarm_t *a = &in->alarms[ev->alarm];
    if (!a->used || a->seq != ev->seq) return;   /* cancelled or re-armed since */
    if ((a->flags & XIP_ALARM_CONNECTED) && !net_connected) { a->waiting_net = true; return; }
    int64_t late = (esp_timer_get_time() - a->due_us) / 1000;
    in->stats.alarms_fired++;
    const uint32_t seq = a->seq;
    const int rc = dispatch(in, a->component, a->method, late < 0 ? 0 : late > INT32_MAX ? INT32_MAX : (int)late, 0, NULL, 0);
    if (settle(in, rc)) return;
    if (!a->used || a->seq != seq) return;       /* the handler cancelled or re-armed it */
    if (!a->interval_ms) { a->used = false; return; }
    a->due_us += (int64_t)a->interval_ms * 1000;
    int64_t left = (a->due_us - esp_timer_get_time()) / 1000;
    alarm_arm(in, ev->alarm, fire_delay_ms(left < 0 ? 0 : (uint32_t)left, a->window_ms, a->flags));
}

static void handle_request(struct xip_instance *in, const xip_event_t *ev)
{
    if (!in->response) in->response = malloc(CONFIG_XIP_RESPONSE_MAX);
    portENTER_CRITICAL(&registry_lock);
    const bool wanted = in->waiter_sem != NULL;
    portEXIT_CRITICAL(&registry_lock);
    in->in_request = true; in->responded = false; in->response_len = 0;
    /* a waiting task asked: the module answers with `send` to the application carrying XIP_REQUEST_ID, which h_send hands to the waiter */
    const int rc = dispatch_packet(in, ev->component, ev->method, in->inbox, in->inbox_len, 0, XIP_ADDR_MAIN, wanted ? XIP_REQUEST_ID : 0,
                                   wanted ? 1u : 0u, 0, 0, NULL);
    in->in_request = false;
    portENTER_CRITICAL(&registry_lock);
    in->inbox_full = false;
    SemaphoreHandle_t waiter = in->waiter_sem;
    in->waiter_sem = NULL;
    if (waiter) {
        size_t n = in->responded ? in->response_len : 0;
        if (n > in->waiter_cap) { n = in->waiter_cap; *in->waiter_rc = XIP_ERR_SIZE; } else *in->waiter_rc = XIP_OK;
        if (n) memcpy(in->waiter_buf, in->response, n);
        *in->waiter_len = n;
    }
    portEXIT_CRITICAL(&registry_lock);
    if (waiter) xSemaphoreGive(waiter);
    settle(in, rc);
}

/* Deliver what is waiting, one call each: the packet is lent for the call and goes back to the pool when the handler
 * returns, unless the module kept it. */
static void handle_packet(const packet_delivery *packet);
static void handle_unbound_packet(const packet_delivery *packet)
{
        int error=0;struct xip_instance *target=request_target(packet->component,(uint32_t)packet->method,&error);
        if(target && packet->component<0){
            if(atomic_load(&quic_module_owner)==target->id){
                const xip_rx_info info={.link=(int)packet->from,.source=packet->id,.signal=packet->value,.time_us=packet->time_us};
                if(quic_module_offer(&info,packet->slot,packet->start,packet->len))return;
            }
            drop_slot(packet->slot);return;
        }
        if(target){packet_delivery resolved=*packet;resolved.owner=target->id;resolved.component=target->component;resolved.method=0;handle_packet(&resolved);return;}
        if(packet->lflags&XIP_CALL_ENVELOPE){
            xip_envelope env;uint8_t *data=xip_pool_slot(pool,packet->slot)+packet->start;
            if(xip_envelope_parse(data,packet->len,&env)==XIP_ENV_REQUEST && env.has_id){
                size_t n=xip_envelope_head(data,XIP_PKT_MAX,env.component,env.method,env.id,error);
                uint16_t start=packet->start,len=(uint16_t)n;
                if(n && quic_reply_prefix(packet->from,packet->id,env.id,packet->slot,&start,&len)>=0){
                    int rc=link_tx(packet->from,packet->slot,start,len,packet->id,0,0,0);
                    quic_reply_finish(packet->from,packet->id,env.id);if(!rc)return;
                }
            }
        }
        drop_slot(packet->slot);return;
}

static void handle_packet(const packet_delivery *packet)
{
    if(!packet->owner){handle_unbound_packet(packet);return;}
    struct xip_instance *in=find(packet->owner);
    if(!in){
        portENTER_CRITICAL(&registry_lock);PACKET_COUNT(&packet_queue,owner_stopped);portEXIT_CRITICAL(&registry_lock);
        drop_slot(packet->slot);return;
    }
    const int slot=packet->slot;
    const uint16_t start=packet->start,len=packet->len;
    int32_t component=packet->component,method=packet->method;
    uint32_t from=packet->from,lflags=packet->lflags,time_us=packet->time_us;
    uint64_t id=packet->id;int32_t value=packet->value;
    portENTER_CRITICAL(&registry_lock);PACKET_COUNT(&packet_queue,dispatched);portEXIT_CRITICAL(&registry_lock);
    const uint8_t *data = xip_pool_slot(pool, slot) + start;
    size_t dlen = len;
    bool envelope = false;
    if (lflags & XIP_CALL_ENVELOPE) {
        /* a 0x444d0002 datagram (xip_quic_common_rx routed it to this module: the component's own, or the console for the native components): the runtime
         * parses the envelope and the module gets an ordinary request. Module ports and everything else deliver raw frames. */
        lflags &= ~(uint32_t)XIP_CALL_ENVELOPE;
        xip_envelope env;
        const xip_envelope_kind kind = xip_envelope_parse(data, len, &env);     /* fields that are not a map (the NOW bench's pattern): a raw frame */
        if (kind == XIP_ENV_REQUEST) {
            /* Only decoded, directed envelope requests vote; raw link frames do not. */
            (void)c_power_received(NULL,data,len,1);
            envelope = true;
            component = env.component; method = env.method;
            data = env.fields; dlen = env.fields_len;
            lflags = XIP_CALL_ENVELOPE;
            env_add(in, from, id, env.component, env.method, env.has_id ? env.id : 0);
            in->req_id = env.has_id ? env.id : 0; in->req_has_id = env.has_id;
        }
    }
    uint32_t h = xip_packet_handle(pool,slot,start,len);
    if (!h) { drop_slot(slot); in->stats.pkt_dropped++; return; }
    in->stats.pkt_received++;
    const bool wants = envelope || (id != 0 && !(lflags & XIP_CALL_LINK));      /* a frame of a link has an address token in `id`, not a request to answer */
    const int rc = dispatch_packet(in, component, method, data, dlen, h, from, id,
                                   (wants ? 1u : 0u) | lflags, value, time_us,
                                   packet->src.port ? &packet->src : NULL);
    in->req_id = 0; in->req_has_id = 0;
    /* Ownership moved to the handler. Rust's packet guard releases an
     * unretained packet; C handlers must release or transfer it themselves. */
    (void)settle(in, rc);
}

/* IDF's USJ API has a SOF state query, no connect/disconnect notification.
 * Run policy on the serialized owner, never in an ISR or the serial task. */
static void nan_usb_changed(void)
{
    static uint32_t notified_instance;
    static bool notified_usb;
    const bool connected = xip_serial_usb_active();
    xSemaphoreTakeRecursive(exec_lock, portMAX_DELAY);
    int c = 0;
    struct xip_instance *p = provider_locked("nan", &c);
    if (p && (p->id != notified_instance || connected != notified_usb)) {
        const xip_nan *nan = component_of_module("nan");
        if (nan && nan->size >= sizeof(*nan) && nan->usb && nan->usb(NULL, connected) == XIP_OK) {
            notified_instance = p->id;
            notified_usb = connected;
        }
    }
    xSemaphoreGiveRecursive(exec_lock);
}

static void worker(void *arg)
{
    (void)arg;
    xip_event_t ev;
    for (;;) {
        nan_usb_changed();
        if (atomic_exchange(&wake_pending, false)) {
            ev = (xip_event_t){.kind = XIP_EV_WAKE, .value = atomic_load(&wake_value)};
        } else if (atomic_exchange(&wifi_pending, false)) {
            ev = (xip_event_t){.kind = XIP_EV_WIFI, .value = (int32_t)atomic_load(&wifi_generation)};
        } else {
            (void)packet_deliveries_prepare();
            if(xQueueReceive(worker_queue,&ev,0)!=pdTRUE){
                packet_delivery packet;
                portENTER_CRITICAL(&registry_lock);
                bool was_full=packet_queue.count==packet_queue.capacity;
                bool ready=packet_queue_pop(&packet_queue,&packet);
                portEXIT_CRITICAL(&registry_lock);
                if(ready){
                    if(was_full && hooks.inbox_ready){
                        /* A shared queue opening can unblock any target. */
                        for(int k=0;k<XIP_MAX_INSTANCES;k++)if(registry[k])hooks.inbox_ready(hooks.packet_ctx,registry[k]->component);
                    }
                    handle_packet(&packet);continue;
                }
                if(xQueueReceive(worker_queue,&ev,pdMS_TO_TICKS(250))!=pdTRUE)continue;
            }
            if (ev.kind == XIP_EV_WAKE) {
                if (!atomic_exchange(&wake_pending, false)) continue;
                ev.value = atomic_load(&wake_value);
            } else if (ev.kind == XIP_EV_WIFI) {
                if (!atomic_exchange(&wifi_pending, false)) continue;
                ev.value = (int32_t)atomic_load(&wifi_generation);
            }
        }
        if (ev.kind == XIP_EV_SLEEP) { xip_power_cycle(); continue; }
        if (ev.kind == XIP_EV_NETWORK || ev.kind == XIP_EV_WAKE || ev.kind == XIP_EV_WIFI) {
            if (ev.kind == XIP_EV_NETWORK && ev.value && quic_active()) xip_quic_udp_ensure();
            if (ev.kind == XIP_EV_NETWORK && ev.value) xip_mc_ensure();
            if (ev.kind == XIP_EV_WIFI && hooks.wifi_reconcile) hooks.wifi_reconcile();
            for (int i = 0; i < XIP_MAX_INSTANCES; i++) {
                struct xip_instance *in = registry[i];
                if (!in) continue;
                if (ev.kind == XIP_EV_WIFI) {
                    (void)settle(in, dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_WIFI, ev.value, 0, NULL, 0));
                } else if (ev.kind == XIP_EV_WAKE) {
                    bool want = false;
                    for (int k = 0; k < XIP_ALARMS; k++) want |= in->alarms[k].used && (in->alarms[k].flags & XIP_ALARM_WINDOW);
                    if (!want) continue;
                    /* NAN's hosted method shares exactly the remote/lifecycle policy.
                     * Other modules retain their generic HOST_WAKE notification. */
                    bool native_nan = false;
                    for (int c = 0; c < XIP_COMPONENTS_MAX; c++) {
                        if (in->comps[c].table && !strcmp(in->comps[c].name, "nan")) {
                            const xip_nan *nan = component_of_module("nan");
                            if (nan && nan->size >= sizeof(*nan) && nan->wake) {
                                (void)nan->wake(NULL, ev.value);
                                native_nan = true;
                            }
                            break;
                        }
                    }
                    if (!native_nan && settle(in, dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_WAKE, ev.value, 0, NULL, 0))) continue;
                } else {
                    if (ev.value) stream_accept(in);
                    if (settle(in, dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_NETWORK, ev.value, 0, NULL, 0))) continue;
                    if (!ev.value) continue;
                    for (int k = 0; k < XIP_ALARMS; k++) {
                        xip_alarm_t *a = &in->alarms[k];
                        if (!a->used || !a->waiting_net) continue;
                        a->waiting_net = false;
                        a->due_us = esp_timer_get_time();
                        alarm_arm(in, k, fire_delay_ms(0, a->window_ms, a->flags));
                    }
                }
            }
            if (ev.kind==XIP_EV_WAKE && ev.value==8) xip_serial_ack_awake();
            continue;
        }
        if(ev.kind==XIP_EV_PACKET)continue; /* coalesced FIFO wake */
        struct xip_instance *in = find(ev.inst);
        if (!in) continue;   /* ended since the event was queued */
        switch (ev.kind) {
        case XIP_EV_START:
            instance_start(in);
            break;
        case XIP_EV_CALL:
            if (ev.method == -1 && ev.component == XIP_HOST_COMPONENT) {   /* stop request */
                dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_STOP, 0, 0, NULL, 0);
                finish(in, 0);
            } else {
                settle(in, dispatch(in, ev.component, ev.method, ev.value, 0, NULL, 0));
            }
            break;
        case XIP_EV_REQUEST: handle_request(in, &ev); break;
        case XIP_EV_LINK: {
            xip_call call = {.size = sizeof(call), .component = ev.component, .method = ev.method, .value = ev.value, .from = ev.from,
                                .id = ev.id, .flags = ev.flags};
            (void)settle(in, dispatch_x(in, &call));
            break;
        }
        case XIP_EV_IO: {
            if (ev.value >= XIP_SOCKET_EVENT_OPEN && ev.value <= XIP_SOCKET_EVENT_WRITE_DONE) socket_delivered((uint32_t)ev.id,ev.value);
            xip_call call = {.size = sizeof(call), .component = ev.component, .method = ev.method,
                                .value = ev.value, .id = ev.id};
            (void)settle(in, dispatch_x(in, &call));
            break;
        }
        case XIP_EV_ALARM: handle_alarm(in, &ev); break;
        default: break;
        }
    }
}

bool IRAM_ATTR xip_post_isr(const xip_event_t *ev, BaseType_t *higher) { return xQueueSendFromISR(worker_queue, ev, higher) == pdTRUE; }
bool xip_connected(void) { return net_connected; }

uint32_t xip_host_stack_high_water(void) { return worker_task ? (uint32_t)uxTaskGetStackHighWaterMark(worker_task) : 0; }

esp_err_t xip_host_init(const xip_host_hooks_t *h)
{
    if (inited) return ESP_OK;
    if (h) hooks = *h;
    extern void xip_wifi_intent_install(xip_host_hooks_t *);
    xip_wifi_intent_install(&hooks);
    xip_periph_reserve((h ? h->reserved_pins : 0) | xip_serial_pins());
    if (inited) return ESP_OK;
    if (xip_info_init()) return ESP_ERR_NO_MEM;
    exec_lock = xSemaphoreCreateRecursiveMutexStatic(&exec_lock_buf);
    for (int i = 0; i < XIP_THUNKS; i++) thunk_slots[i].table = -1;
    if(!packet_deliveries_prepare())return ESP_ERR_NO_MEM;
    worker_queue = xQueueCreate(CONFIG_XIP_EVENT_QUEUE, sizeof(xip_event_t));
    if (!worker_queue) return ESP_ERR_NO_MEM;
    if (xTaskCreate(worker, "xip", CONFIG_XIP_WORKER_STACK, NULL, CONFIG_XIP_WORKER_PRIORITY, &worker_task) != pdPASS)
        return ESP_ERR_NO_MEM;
    xip_periph_init();
    inited = true;
    /* the wire: records on, from the very first byte. Without it there is no
     * console and no module traffic, so the host does not come up at all. */
    if (xip_serial_start()) {
        inited = false;
        ESP_LOGE(TAG, "the serial did not start");
        return ESP_FAIL;
    }
    /* the link the records travel on: registered with the wire, not when a module binds the console
     * port — a record of any port a module holds must reach it, with or without a console. */
    serial_link = xip_link_native_register("serial", XIP_PKT_MAX, XIP_LINK_FLAG_STREAM, serial_tx, NULL);
    if (serial_link < 0) {
        inited = false;
        ESP_LOGE(TAG, "the serial link did not register");
        return ESP_FAIL;
    }
    (void)xip_link_native_set_state(serial_link, XIP_LINK_UP, 0);
    xip_wifi_intent initial = {.size = sizeof(initial)}; uint32_t generation;
    if (hooks.wifi_get && !hooks.wifi_get(&initial, &generation)) xip_host_wifi_changed(generation);
    return ESP_OK;
}

esp_err_t xip_host_init_simple(const xip_host_ext_t *ext)
{
    xip_host_hooks_t h = {0};
    if (ext) {
        h.wifi_set = ext->wifi_set; h.wifi_get = ext->wifi_get; h.wifi_state = ext->wifi_state;
        h.nv = ext->nv; h.pool = ext->pool; h.wifi_scan = ext->wifi_scan; h.wifi_monitor = ext->wifi_monitor;
        h.modules_info = ext->modules_info; h.module_start = ext->module_start; h.module_stop = ext->module_stop;
        h.module_manifest = ext->module_manifest; h.module_rescan = ext->module_rescan; h.component_start = ext->component_start; h.request_start = ext->request_start;
        h.wifi_reconcile = ext->wifi_reconcile;
        h.wifi_tuning_set = ext->wifi_tuning_set; h.wifi_tuning_get = ext->wifi_tuning_get;
        h.packet = ext->packet; h.inbox_ready = ext->inbox_ready;
    }
    return xip_host_init(&h);
}

uint8_t xip_host_network_sockets(void)
{
    unsigned count=xip_quic_udp_sockets()+xip_mc_sockets();
    if(sockets_lock){
        xSemaphoreTake(sockets_lock,portMAX_DELAY);
        for(sock_node *s=sockets;s;s=s->next)count++;
        xSemaphoreGive(sockets_lock);
    }
    return count>255 ? 255 : (uint8_t)count;
}

void xip_host_network_suspend(void)
{
    xip_quic_udp_stop();
    xip_mc_stop();
    socket_network_down();
    xip_host_set_connected(false);
}

void xip_host_set_connected(bool connected)
{
    if (!inited) return;
    portENTER_CRITICAL(&registry_lock);
    const bool changed=net_connected!=connected;
    net_connected=connected;
    portEXIT_CRITICAL(&registry_lock);
    if (changed) {
        xip_event_t ev={.kind=XIP_EV_NETWORK,.value=connected};
        (void)xQueueSend(worker_queue,&ev,0);
    }
}

int xip_power_post(void)
{
    if (!inited) return XIP_ERR_UNSUPPORTED;
    xip_event_t ev = {.kind = XIP_EV_SLEEP};
    return xQueueSend(worker_queue, &ev, 0) == pdTRUE ? XIP_OK : XIP_ERR_BUSY;
}

void xip_host_wake_window_value(int32_t value)
{
    if (!inited) return;
    atomic_store(&wake_value, value);
    atomic_store(&wake_pending, true);
    /* Coalesce UART chunks. Even if the queue is full the worker observes the
     * pending wake before taking another event; activity cannot be lost. */
    xip_event_t ev = {.kind = XIP_EV_WAKE};
    (void)xQueueSend(worker_queue, &ev, 0);
}

void xip_host_wake_window(void) { xip_host_wake_window_value(0); }

void xip_host_wifi_changed(uint32_t generation)
{
    if (!inited) return;
    atomic_store(&wifi_generation, generation);
    atomic_store(&wifi_pending, true);
    xip_event_t ev = {.kind = XIP_EV_WIFI};
    (void)xQueueSend(worker_queue, &ev, 0);
}


xip_instance_t *xip_instance_create(const char *name, const xip_instance_config_t *cfg)
{
    if (!inited || !name || !*name || !cfg || !cfg->handler || cfg->state_bytes > 4096) return NULL;
    for (int a = 0; a < XIP_COMPONENTS_MAX; a++) {
        const int component = cfg->envelope_components[a];
        if (component < 0 || (component && component == cfg->component)) return NULL;
        for (int b = 0; component && b < a; b++) if (cfg->envelope_components[b] == component) return NULL;
    }
    struct xip_instance *in = calloc(1, sizeof(*in));
    if (!in) return NULL;
    strlcpy(in->name, name, sizeof(in->name));
    in->handler = cfg->handler; in->done = cfg->done; in->done_ctx = cfg->done_ctx;
    in->state_bytes = cfg->state_bytes;
    in->component = cfg->component;
    in->port = cfg->port;
    in->stream = cfg->stream;
    memcpy(in->envelope_components, cfg->envelope_components, sizeof(in->envelope_components));
    for (int c = 0; c < XIP_COMPONENTS_MAX; c++) strlcpy(in->comps[c].name, cfg->components[c], sizeof(in->comps[c].name));
    xip_mem_init(&in->mem, CONFIG_XIP_MEM_QUOTA ? CONFIG_XIP_MEM_QUOTA : UINT32_MAX);
    in->state = calloc(1, cfg->state_bytes ? cfg->state_bytes : 1);
    in->inbox = malloc(CONFIG_XIP_REQUEST_MAX);
    if (!in->state || !in->inbox) { free(in->state); free(in->inbox); free(in); return NULL; }
    in->table = (xip_host){
        .device_info = h_device_info, .info_publish = h_info_publish, .get_component = h_get_component,
        .abi_version = XIP_ABI_VERSION,
        .size = sizeof(xip_host),
        .user = in,
        .log = h_log, .now_ms = h_now_ms, .now_us = h_now_us,
        .get_setting = h_get_setting, .set_setting = h_set_setting,
        .alarm_set = h_alarm_set, .alarm_cancel = h_alarm_cancel,
        .modules_info = h_modules_info, .module_start = h_module_start, .module_stop = h_module_stop,
        .reset = h_reset,
        .pkt_alloc = h_pkt_alloc, .pkt_buf = h_pkt_buf, .pkt_keep = h_pkt_keep, .pkt_free = h_pkt_free, .send = h_send,
        .pkt_info = h_pkt_info, .mem_alloc = h_mem_alloc, .mem_free = h_mem_free, .pkt_span = h_pkt_span,
        .socket_read = h_socket_read, .socket_write = h_socket_write,
        .socket_close = h_socket_close, .socket_peer = h_socket_peer, .socket_write_pkt = h_socket_write_pkt,
        .udp_send = h_udp_send,
        .flash_erase = h_flash_erase, .flash_write = h_flash_write, .flash_read = h_flash_read, .pkt_pool_info=h_pkt_pool_info, .pkt_acquire=h_pkt_acquire, .pkt_tx=h_pkt_tx, .module_manifest = h_module_manifest, .module_rescan = h_module_rescan, .nv = h_nv, .heap_info = h_heap_info, .heap_dump = h_heap_dump,
    };
    for (int i = 0; i < XIP_IRQ_SLOTS; i++) in->irq[i].inst = in;
    portENTER_CRITICAL(&registry_lock);
    int slot = -1;
    for (int i = 0; i < XIP_MAX_INSTANCES && slot < 0; i++) if (!registry[i]) slot = i;
    for (int i = 0; i < XIP_MAX_INSTANCES && slot >= 0; i++) {
        struct xip_instance *other = registry[i];
        if (!other) continue;
        for (int a = 0; a < XIP_COMPONENTS_MAX; a++) {
            const int own = in->envelope_components[a], theirs = other->envelope_components[a];
            if ((own && own == other->component) || (theirs && theirs == in->component)) slot = -1;
            for (int b = 0; own && b < XIP_COMPONENTS_MAX; b++)
                if (own == other->envelope_components[b]) slot = -1;
        }
    }
    if (slot >= 0) { in->slot = slot; in->id = next_id++; registry[slot] = in; }
    portEXIT_CRITICAL(&registry_lock);
    if (slot < 0) { free(in->state); free(in->inbox); free(in); return NULL; }
    if (in->port) {
        int rc = port_bind(in);
        if (rc) ESP_LOGW(TAG, "%s: port %u not bound: %d", in->name, (unsigned)in->port, rc);
    }
    stream_accept(in);
    return in;
}

const xip_host *xip_instance_host(const xip_instance_t *inst) { return inst ? &inst->table : NULL; }

static int queue_event(struct xip_instance *in, xip_event_t *ev)
{
    ev->inst = in->id;
    return xQueueSend(worker_queue, ev, 0) == pdTRUE ? 0 : XIP_ERR_BUSY;
}

int xip_instance_start(xip_instance_t *in, const uint8_t *data, size_t len)
{
    if (!in || (len && !data)) return XIP_ERR_ARG;
    uint8_t *copy = len ? malloc(len) : NULL;
    if (len && !copy) return XIP_ERR_NOMEM;
    if (len) memcpy(copy, data, len);
    int rc = XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in) && !in->start_request) { in->start_request = copy; in->start_len = len; rc = 0; copy = NULL; }
    portEXIT_CRITICAL(&registry_lock);
    free(copy);
    if (rc) return rc;
    xip_event_t ev = {.kind = XIP_EV_START};
    return queue_event(in, &ev);
}

int xip_instance_stop(xip_instance_t *in)
{
    if (!in) return XIP_ERR_ARG;
    portENTER_CRITICAL(&registry_lock);
    const bool ok = known_locked(in);
    const uint32_t id = ok ? in->id : 0;
    portEXIT_CRITICAL(&registry_lock);
    if (!ok) return XIP_ERR_NOTFOUND;
    xip_event_t ev = {.kind = XIP_EV_CALL, .inst = id, .component = XIP_HOST_COMPONENT, .method = -1};
    return xQueueSend(worker_queue, &ev, 0) == pdTRUE ? 0 : XIP_ERR_BUSY;
}

static int post_locked_copy(struct xip_instance *in, int component, int method, const uint8_t *data, size_t len,
                            SemaphoreHandle_t sem, uint8_t *buf, size_t cap, size_t *out_len, int *rc_out)
{
    int rc = XIP_ERR_NOTFOUND;
    uint32_t id = 0;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in)) {
        if (in->inbox_full) rc = XIP_ERR_BUSY;
        else {
            if (len) memcpy(in->inbox, data, len);
            in->inbox_len = len;
            in->inbox_full = true;
            if (sem) { in->waiter_sem = sem; in->waiter_buf = buf; in->waiter_cap = cap; in->waiter_len = out_len; in->waiter_rc = rc_out; }
            id = in->id;
            rc = 0;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if (rc) return rc;
    xip_event_t ev = {.kind = XIP_EV_REQUEST, .inst = id, .component = component, .method = method};
    if (xQueueSend(worker_queue, &ev, 0) != pdTRUE) {
        portENTER_CRITICAL(&registry_lock);
        if (known_locked(in)) { in->inbox_full = false; if (sem) in->waiter_sem = NULL; }
        portEXIT_CRITICAL(&registry_lock);
        return XIP_ERR_BUSY;
    }
    return 0;
}

int xip_instance_post(xip_instance_t *in, int component, int method, const uint8_t *data, size_t len)
{
    if (!in || (len && !data)) return XIP_ERR_ARG;
    if (len > CONFIG_XIP_REQUEST_MAX) return XIP_ERR_SIZE;
    return post_locked_copy(in, component, method, data, len, NULL, NULL, 0, NULL, NULL);
}

int xip_instance_request(xip_instance_t *in, int component, int method, const uint8_t *data, size_t len,
                            uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms)
{
    if (!in || (len && !data) || (capacity && !response) || !response_len) return XIP_ERR_ARG;
    if (len > CONFIG_XIP_REQUEST_MAX) return XIP_ERR_SIZE;
    *response_len = 0;
    SemaphoreHandle_t sem = xSemaphoreCreateBinary();
    if (!sem) return XIP_ERR_NOMEM;
    int result = XIP_ERR_TIMEOUT;
    int rc = post_locked_copy(in, component, method, data, len, sem, response, capacity, response_len, &result);
    if (!rc && xSemaphoreTake(sem, pdMS_TO_TICKS(timeout_ms ? timeout_ms : 1000)) != pdTRUE) {
        portENTER_CRITICAL(&registry_lock);
        const bool pending = known_locked(in) && in->waiter_sem == sem;
        if (pending) in->waiter_sem = NULL;   /* the worker will not touch the caller's buffers any more */
        portEXIT_CRITICAL(&registry_lock);
        if (!pending) (void)xSemaphoreTake(sem, portMAX_DELAY);   /* it was just answered: the give is on its way */
        else result = XIP_ERR_TIMEOUT;
    } else if (rc) {
        result = rc;
    }
    vSemaphoreDelete(sem);
    return result;
}

void xip_instance_stats(const xip_instance_t *in, xip_stats_t *out)
{
    if (!out) return;
    memset(out, 0, sizeof(*out));
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in)) *out = in->stats;
    portEXIT_CRITICAL(&registry_lock);
}
