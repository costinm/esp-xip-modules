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
#include "internal.h"
#include "xip_wifi.h"

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
                           uint32_t from, uint64_t id, uint32_t flags, int32_t value, uint32_t time_us);
static const xip_link *link_table(void);
static void links_release(struct xip_instance *in);
static int link_send(struct xip_instance *in, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t token);
static void link_tx_returned(uint32_t from, int slot);
static void serial_release(struct xip_instance *in);
static int h_tcp_accept(void *u, uint16_t port, uint32_t mode, int32_t c, int32_t m, uint32_t *handle);
static xip_pool *pool_get(void);
static void drop_slot(int slot);
static bool link_drop_pending(int slot);
static void wake_reader_waiters(void);
static uint32_t now_us32(void);
static void link_fanout(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us);
static struct xip_instance *by_component(int component, const struct xip_instance *except);
static bool known_locked(const struct xip_instance *in);

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

static uint64_t h_now_ms(void *user) { (void)user; return (uint64_t)(esp_timer_get_time() / 1000); }

int xip_wifi_set_intent(const xip_wifi_intent *i, uint32_t *gen) { return hooks.wifi_set ? hooks.wifi_set(i, gen) : XIP_ERR_UNSUPPORTED; }
int xip_wifi_get_intent(xip_wifi_intent *i, uint32_t *gen) { return hooks.wifi_get ? hooks.wifi_get(i, gen) : XIP_ERR_UNSUPPORTED; }
int xip_wifi_get_tuning(xip_wifi_tuning *t) { return hooks.wifi_tuning_get ? hooks.wifi_tuning_get(t) : XIP_ERR_UNSUPPORTED; }

static atomic_bool wifi_pending;
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
    if (!st || st->size < sizeof(*st)) return XIP_ERR_ARG;
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
static const xip_wifi wifi_component = {
    .version = 1, .size = sizeof(xip_wifi), .set = c_wifi_set, .get = c_wifi_get, .state = c_wifi_state,
    .scan = c_wifi_scan, .monitor = c_wifi_monitor,
};

/* get_component("power") */
static int c_power_sleep(void *ctx, const xip_power_sleep *r) { (void)ctx; return xip_power_request(r); }
static int c_power_info(void *ctx, xip_power_info *i) { (void)ctx; return xip_power_get_info(i); }
static int c_power_dump_locks(void *ctx) { (void)ctx; return xip_power_dump(0); }
static int c_power_dump_tasks(void *ctx) { (void)ctx; return xip_power_dump(1); }
static const xip_power power_component = {
    .version = 1, .size = sizeof(xip_power), .sleep = c_power_sleep, .info = c_power_info,
    .dump_locks = c_power_dump_locks, .dump_tasks = c_power_dump_tasks,
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
    if (!hooks.nv) return XIP_ERR_UNSUPPORTED;
    if (!key || !key_len || key_len > 32 || (value_len && !value) || (!value && (!out || !capacity))) return XIP_ERR_ARG;
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
    /* written synchronously: DONE returns the packet to its sender or the pool. A failed write is an error, as for every link: the
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
int xip_port_rx_begin(uint32_t port, xip_link_rx_buf *buf)
{
    xip_pool *p = pool_get();
    if (!p || !buf) return XIP_ERR_ARG;
    if (!port_bound(port)) return 1;
    int slot = xip_pool_acquire(p, 0);
    if (slot < 0) return XIP_ERR_NOMEM;
    buf->slot = slot;
    buf->data = xip_pool_slot(p, slot) + XIP_PKT_HEADROOM;
    buf->capacity = XIP_PKT_MAX;
    return XIP_OK;
}

void xip_port_rx_commit(int link, uint32_t port, const xip_link_rx_buf *buf, size_t len, uint64_t token)
{
    if (len > XIP_PKT_MAX || link < 0) { drop_slot(buf->slot); return; }
    link_fanout((uint32_t)link, port, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, token, 0, now_us32());
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

int xip_device_info(uint8_t *out, size_t capacity) { return xip_rt_device_info(&hooks, out, capacity); }
static int h_device_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user; return xip_device_info(out, capacity);
}
static int h_info_publish(void *user, int32_t component, int32_t method, const uint8_t *data, size_t len)
{
    return xip_rt_info_publish(I(user)->id, component, method, data, len);
}

static const void *h_get_component(void *user, const char *name)
{
    (void)user; /* Requester context; native component functions take no extra argument. */
    extern const xip_psa *xip_psa_component(void);
    if (!name) return NULL;
    if (strcmp(name, "psa") == 0) return xip_psa_component();
    if (strcmp(name, "link") == 0) return link_table();
    if (strcmp(name, "power") == 0) return &power_component;
    if (strcmp(name, "hw") == 0) return xip_periph_table();
    if (strcmp(name, "wifi") == 0) return hooks.wifi_set ? &wifi_component : NULL;
    if (strcmp(name, "wifi-tuning") == 0) return hooks.wifi_tuning_set ? &tuner_component : NULL;
    return NULL;
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

static xip_pool *pool;
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

static void drop_slot(int slot)
{
    /* Dropping a provider's sole TX lease is an aborted transmission, not an
     * orphaned completion record or a second runtime-owned packet reference. */
    if (xip_pool_refs(pool, slot) == 1 && link_drop_pending(slot)) return;
    const bool shared = xip_pool_refs(pool, slot) > 1;
    xip_pool_release(pool, slot);
    if (shared && xip_pool_refs(pool, slot) == 1) wake_reader_waiters();
}

static int h_pkt_alloc(void *user, uint32_t *out)
{
    struct xip_instance *in = I(user);
    xip_pool *p = pool_get();
    if (!out) return XIP_ERR_ARG;
    *out = 0;
    if (!p) return XIP_ERR_NOMEM;
    if (xip_held_count(&in->held, XIP_HELD_OWNED) >= in->held.quota) return XIP_ERR_BUSY;
    int slot = xip_pool_acquire(p, CONFIG_XIP_PKT_RESERVE);
    if (slot < 0) return XIP_ERR_NOMEM;
    uint32_t h = xip_held_add(&in->held, slot, XIP_PKT_HEADROOM, 0, XIP_HELD_OWNED);
    if (!h) { drop_slot(slot); return XIP_ERR_BUSY; }
    *out = h;
    return XIP_OK;
}

static int h_pkt_buf(void *user, uint32_t pkt, uint8_t **ptr, size_t *max)
{
    struct xip_instance *in = I(user);
    xip_held_entry *e = xip_held_get(&in->held, pkt);
    if (!e || !ptr || !max) return XIP_ERR_ARG;
    if (xip_pool_refs(pool, e->slot) != 1) return XIP_ERR_BUSY;     /* shared: something else still reads it */
    *ptr = xip_pool_slot(pool, e->slot) + e->start;
    *max = xip_pool_slot_size(pool) - e->start - PKT_TAIL;
    if (*max > XIP_PKT_MAX) *max = XIP_PKT_MAX;
    return XIP_OK;
}

static int h_pkt_span(void *user, uint32_t pkt, uint8_t **base, size_t *capacity)
{
    struct xip_instance *in = I(user);
    xip_held_entry *e = xip_held_get(&in->held, pkt);
    if (!e || !base || !capacity || !pool) return XIP_ERR_ARG;
    if (xip_pool_refs(pool, e->slot) != 1) return XIP_ERR_BUSY;
    *base = xip_pool_slot(pool, e->slot);
    *capacity = xip_pool_slot_size(pool);
    return XIP_OK;
}

/* Generic HAL implementations; no upload format or loader policy. */
#include "socket_hal.inc"
#include "flash_hal.inc"

static int h_pkt_keep(void *user, uint32_t pkt)
{
    int rc = xip_held_keep(&I(user)->held, pkt);
    return rc == 0 ? XIP_OK : rc == -2 ? XIP_ERR_BUSY : XIP_ERR_ARG;
}

static int h_pkt_free(void *user, uint32_t pkt)
{
    xip_held_entry e = xip_held_take(&I(user)->held, pkt);
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

/* The packet goes to the target's inbox ring and a worker event; on success the target owns the reference. */
static int deliver(struct xip_instance *t, int slot, uint16_t start, uint16_t len, int component, int method,
                   uint32_t from, uint64_t id, int32_t value, uint32_t lflags, uint32_t time_us)
{
    uint32_t tid = 0;
    int rc = XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(t)) {
        if (t->pkt_count >= CONFIG_XIP_PKT_INBOX) rc = XIP_ERR_BUSY;
        else {
            unsigned at = (t->pkt_head + t->pkt_count++) % CONFIG_XIP_PKT_INBOX;
            t->pkt_in[at].slot = (int16_t)slot; t->pkt_in[at].start = start; t->pkt_in[at].len = len;
            t->pkt_in[at].component = component; t->pkt_in[at].method = method;
            t->pkt_in[at].from = from; t->pkt_in[at].id = id;
            t->pkt_in[at].value = value; t->pkt_in[at].lflags = lflags; t->pkt_in[at].time_us = time_us;
            tid = t->id;
            rc = 0;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if (rc) return rc;
    xip_event_t ev = {.kind = XIP_EV_PACKET, .inst = tid};
    if (xQueueSend(worker_queue, &ev, 0) == pdTRUE) return 0;
    portENTER_CRITICAL(&registry_lock);              /* no room for the event: take the packet back out */
    if (known_locked(t)) {
        for (unsigned k = 0; k < t->pkt_count; k++) {
            unsigned at = (t->pkt_head + k) % CONFIG_XIP_PKT_INBOX;
            if (t->pkt_in[at].slot != slot) continue;
            for (unsigned j = k; j + 1 < t->pkt_count; j++)
                t->pkt_in[(t->pkt_head + j) % CONFIG_XIP_PKT_INBOX] = t->pkt_in[(t->pkt_head + j + 1) % CONFIG_XIP_PKT_INBOX];
            t->pkt_count--;
            break;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    return XIP_ERR_BUSY;
}

static int h_send(void *user, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t id)
{
    struct xip_instance *in = I(user);
    if (dst >= XIP_ADDR_LINK) return link_send(in, pkt, len, dst, component, method, id);   /* ERR_BUSY keeps the packet */
    xip_held_entry e = xip_held_take(&in->held, pkt);       /* consumed on success and on failure */
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
    } else {
        struct xip_instance *t = by_component((int)dst, NULL);
        rc = t ? deliver(t, e.slot, e.start, (uint16_t)len, component, method, in->component, id, 0, 0, 0) : XIP_ERR_NOTFOUND;
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
    int rc = deliver(in, slot, XIP_PKT_HEADROOM, (uint16_t)len, component, method, XIP_ADDR_MAIN, id, 0, 0, 0);
    if (rc) { drop_slot(slot); in->stats.pkt_dropped++; }
    return rc;
}

/* ---- links: frames between transports and modules (the `link` component, include/xip.h) ----
 * A link is a name and an address (XIP_ADDR_LINK | index). Its provider is the runtime (xip_link_native_register: a driver) or a
 * module (link.provide); its subscribers are modules (link.open). Frames go from the provider to every subscriber as packets with
 * XIP_CALL_LINK, unless a route (link.route) forwards them first, from the driver's context; `send` to the address goes to the
 * provider and, when the sender asked, the packet comes back as a TX_DONE event. Tables under the registry lock; delivery outside it. */
#define XIP_LINKS_MAX 6
#define XIP_LINK_SUBS 6
#define XIP_LINK_ROUTES 16
#define XIP_TX_PENDING 16
typedef struct {
    bool used;
    char name[16];
    uint8_t provider;                       /* XIP_LINK_NONE / NATIVE / MODULE */
    bool wait_readers;                      /* a TX lease is waiting for read borrows to end */
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

/* TX return metadata only. The native driver or module provider holds the sole
 * lease until completion; this table never owns another reference. */
static struct {
    bool used;
    uint32_t addr;
    int slot, sender, component, method;
    uint16_t start, len;
    uint64_t token;
    int64_t t0_us;
} pending[XIP_TX_PENDING];

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
    if (l) { rc = XIP_OK; subs_remove_locked(l, in->component); }
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
    struct { int owner, component, method; } subs[XIP_LINK_SUBS];
    unsigned n = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) { n = l->nsubs; memcpy(subs, l->subs, n * sizeof(subs[0])); }
    portEXIT_CRITICAL(&registry_lock);
    for (unsigned k = 0; k < n; k++) {
        struct xip_instance *t = by_component(subs[k].owner, NULL);
        if (!t) continue;
        xip_event_t ev = {.kind = XIP_EV_LINK, .inst = t->id, .component = subs[k].component, .method = subs[k].method, .value = value,
                          .id = id, .from = addr, .flags = XIP_CALL_LINK | XIP_CALL_LINK_EVENT | (kind << 8)};
        (void)xQueueSend(worker_queue, &ev, 0);
    }
}

/* READY is a retry hint: a released read borrow may unblock a held TX lease.
 * Marking wait_readers and examining refs in link_tx use this same lock, so a
 * release racing TX admission cannot lose its wake-up. */
static void wake_reader_waiters(void)
{
    uint32_t wake[XIP_LINKS_MAX]; unsigned n = 0;
    portENTER_CRITICAL(&registry_lock);
    for (unsigned i = 0; i < XIP_LINKS_MAX; i++) {
        link_t *l = &links[i];
        if (l->used && l->provider != XIP_LINK_NONE && l->wait_readers) {
            l->wait_readers = false; wake[n++] = link_addr(l);
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    for (unsigned i = 0; i < n; i++) link_event(wake[i], XIP_LINK_EVENT_READY, 0, 0);
}

/* ---- transmit: the sending side of a link ---- */
static bool pending_add(uint32_t addr, int slot, uint16_t start, uint16_t len, int sender, int component, int method, uint64_t token)
{
    bool ok = false;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_TX_PENDING; i++) {
        if (pending[i].used && pending[i].slot == slot) { portEXIT_CRITICAL(&registry_lock); return false; }
    }
    for (int i = 0; i < XIP_TX_PENDING && !ok; i++) {
        if (pending[i].used) continue;
        pending[i].used = true; pending[i].addr = addr; pending[i].slot = slot; pending[i].start = start; pending[i].len = len;
        pending[i].sender = sender; pending[i].component = component; pending[i].method = method; pending[i].token = token;
        pending[i].t0_us = esp_timer_get_time();
        ok = true;
    }
    portEXIT_CRITICAL(&registry_lock);
    return ok;
}
static bool pending_take(int slot, typeof(pending[0]) *out)
{
    bool found = false;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < XIP_TX_PENDING && !found; i++) {
        if (!pending[i].used || pending[i].slot != slot) continue;
        *out = pending[i];
        pending[i].used = false;
        found = true;
    }
    portEXIT_CRITICAL(&registry_lock);
    return found;
}

/* The provider is done with the frame in `slot`: the reference the runtime kept goes back to the sender (TX_DONE) or is released. */
static void link_return(int slot, const typeof(pending[0]) *p, int outcome)
{
    struct xip_instance *t = by_component(p->sender, NULL);
    const uint32_t elapsed = (uint32_t)(esp_timer_get_time() - p->t0_us);
    if (!t || deliver(t, slot, p->start, p->len, p->component, p->method, p->addr, p->token, outcome,
                      XIP_CALL_LINK | XIP_CALL_LINK_EVENT | (XIP_LINK_EVENT_TX_DONE << 8), elapsed)) drop_slot(slot);
}
static void link_complete(int slot, int outcome)
{
    typeof(pending[0]) p;
    if (!pending_take(slot, &p)) { drop_slot(slot); return; }
    link_return(slot, &p, outcome);
}
static bool link_drop_pending(int slot)
{
    typeof(pending[0]) p;
    if (!pending_take(slot, &p)) return false;
    link_return(slot, &p, XIP_ERR_IO);
    return true;
}
static void link_tx_returned(uint32_t from, int slot)
{
    (void)from;
    link_complete(slot, XIP_OK);
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
    /* Mutable TX cannot overlap a shared receive/fan-out borrow. BUSY retains
     * the exact lease; retry after readers release it. No implicit clone. */
    portENTER_CRITICAL(&registry_lock);
    const bool shared = xip_pool_refs(pool, slot) != 1;
    l = link_at_locked(dst);
    if (shared && l) l->wait_readers = true;
    portEXIT_CRITICAL(&registry_lock);
    if (shared) return XIP_ERR_BUSY;
    const bool completion = component || method;
    int rc;
    if (completion && !pending_add(dst, slot, start, len, sender, component, method, token)) return XIP_ERR_BUSY;
    if (provider == XIP_LINK_NATIVE && tx) {
        xip_link_packet pk = {.data = xip_pool_slot(pool, slot) + start, .len = len, .headroom = start, .slot = slot, .pool = pool};
        rc = tx(tx_ctx, token, &pk);
        if (rc < 0) {
            if (completion) { typeof(pending[0]) gone; (void)pending_take(slot, &gone); }
        } else {
            if (rc == XIP_LINK_TX_DONE) link_complete(slot, XIP_OK);     /* sent already: the runtime's reference ends here */
            rc = 0;
        }
    } else if (provider == XIP_LINK_MODULE) {
        struct xip_instance *t = by_component(owner, NULL);
        rc = t ? deliver(t, slot, start, len, tx_component, tx_method, sender, token, 0, XIP_CALL_LINK, 0) : XIP_ERR_NOTFOUND;
        if (rc && completion) { typeof(pending[0]) gone; (void)pending_take(slot, &gone); }
        /* Success moves the sole reference into the provider's inbox. Pending
         * completion is metadata only; it owns no second alias of the packet. */
    } else {
        rc = XIP_ERR_UNSUPPORTED;
        if (completion) { typeof(pending[0]) gone; (void)pending_take(slot, &gone); }
    }
    portENTER_CRITICAL(&registry_lock);
    link_t *l2 = link_at_locked(dst);
    if (l2) { if (rc == 0) l2->sent++; else if (rc != XIP_ERR_BUSY) l2->dropped++; }
    portEXIT_CRITICAL(&registry_lock);
    if (rc == 0 && provider == XIP_LINK_NATIVE && !completion) { /* the driver releases it with tx_done */ }
    return rc;
}

/* `send` to a link address. ERR_BUSY is the one failure that leaves the packet with the sender. */
static int link_send(struct xip_instance *in, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t token)
{
    xip_held_entry *held = xip_held_get(&in->held, pkt);
    if (!held || held->state != XIP_HELD_OWNED) {
        (void)xip_held_take(&in->held, pkt);                             /* a borrowed or unknown handle: consumed, as for any send */
        return XIP_ERR_ARG;
    }
    const int slot = held->slot;
    const uint16_t start = held->start;
    if (len > XIP_PKT_MAX || start + len > xip_pool_slot_size(pool)) {
        (void)xip_held_take(&in->held, pkt);
        drop_slot(slot);
        in->stats.pkt_dropped++;
        return XIP_ERR_SIZE;
    }
    /* Move out of the sender's table before lending TX to the provider. On
     * BUSY restore the same entry/generation, never mint a replacement handle.
     * This table is used only by the serialized module worker. */
    xip_held_entry moved = xip_held_take(&in->held, pkt);
    int rc = link_tx(dst, slot, start, (uint16_t)len, token, in->component, component, method);
    if (rc == XIP_ERR_BUSY) { *held = moved; return rc; }
    if (rc == 0) { in->stats.pkt_sent++; return 0; }
    drop_slot(slot);
    in->stats.pkt_dropped++;
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
    xip_held_entry e = xip_held_take(&in->held, pkt);
    if (e.slot < 0) return XIP_ERR_ARG;
    link_complete(e.slot, outcome);                                      /* move the sole reference back, or release it */
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

/* The packet in `slot` (one reference is the caller's) goes to the route or to every subscriber: one reference each. */
static void link_fanout(uint32_t addr, uint32_t port, int slot, uint16_t start, uint16_t len, uint64_t source, int32_t signal, uint32_t time_us)
{
    struct { int owner, component, method; } subs[XIP_LINK_SUBS];
    unsigned n = 0;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    if (l) l->received++;
    portEXIT_CRITICAL(&registry_lock);
    if (!port && link_route(addr, slot, start, len)) return;
    portENTER_CRITICAL(&registry_lock);
    l = link_at_locked(addr);
    if (port) {            /* the module bound to the port, whichever link the frame came on */
        for (int i = 0; i < XIP_PORTS_MAX; i++)
            if (ports[i].port == port && n < XIP_LINK_SUBS) { subs[n].owner = subs[n].component = ports[i].component; subs[n].method = 0; n++; }
    } else if (l) { n = l->nsubs; memcpy(subs, l->subs, n * sizeof(subs[0])); }
    portEXIT_CRITICAL(&registry_lock);
    unsigned lost = 0;
    if (!n) { drop_slot(slot); return; }
    for (unsigned k = 0; k < n; k++) {
        if (k + 1 < n) xip_pool_retain(pool, slot);          /* the last subscriber takes the caller's reference */
        struct xip_instance *t = by_component(subs[k].owner, NULL);
        if (!t || deliver(t, slot, start, len, subs[k].component, subs[k].method, addr, source, signal, XIP_CALL_LINK, time_us)) {
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
    xip_held_entry e = xip_held_take(&in->held, pkt);            /* consumed on success and on failure */
    if (e.slot < 0) return XIP_ERR_ARG;
    bool mine;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked(addr);
    mine = l && l->provider == XIP_LINK_MODULE && l->owner == in->component;
    portEXIT_CRITICAL(&registry_lock);
    if (!mine) { drop_slot(e.slot); return XIP_ERR_ARG; }
    if (len > XIP_PKT_MAX || e.start + len > xip_pool_slot_size(pool)) { drop_slot(e.slot); return XIP_ERR_SIZE; }
    in->stats.pkt_sent++;
    link_fanout(addr, 0, e.slot, e.start, (uint16_t)len, source, signal, time_us ? time_us : now_us32());
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
        out->subscribers = l->nsubs; out->received = l->received; out->sent = l->sent; out->forwarded = l->forwarded; out->dropped = l->dropped;
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
    /* packets_release moves/frees each provider-held or queued lease once and
     * reports pending TX as IO failure through drop_slot. Do not return a lease
     * here while the provider's held table/inbox still owns it. */
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
void xip_link_native_tx_done(int addr, int slot, int outcome) { (void)addr; link_complete(slot, outcome); }

/* Receive without a copy: the driver parses into a pool packet, the way DMesh's receive_now_with does. begin returns 0 and a
 * packet to fill (capacity XIP_PKT_MAX at the payload window), 1 when nobody listens (skip the frame), or an error (XIP_ERR_NOMEM:
 * the pool is empty, counted). Then commit (the frame is `len` bytes; stamped now) or abort. */
static int native_rx_begin(int addr, xip_link_rx_buf *buf, bool cached)
{
    xip_pool *p = pool_get();
    unsigned subs = 0;
    bool routed = false;
    portENTER_CRITICAL(&registry_lock);
    link_t *l = link_at_locked((uint32_t)addr);
    if (l) { subs = l->nsubs; routed = l->nroutes != 0; }
    portEXIT_CRITICAL(&registry_lock);
    if (!l || !buf || !p) return XIP_ERR_ARG;
    if (!subs && !routed) return 1;
    int slot = cached ? xip_pool_acquire_cached(p, 0) : xip_pool_acquire(p, 0);
    if (slot < 0) {
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
    link_fanout((uint32_t)addr, 0, buf->slot, XIP_PKT_HEADROOM, (uint16_t)len, source, signal, now_us32());
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
    if (l) { subs = l->nsubs; routed = l->nroutes != 0; }
    portEXIT_CRITICAL(&registry_lock);
    if (!l || (len && !data) || !p) return XIP_ERR_ARG;
    if (!subs && !routed) return XIP_OK;                                 /* nobody listens: not worth a packet */
    if (len > XIP_PKT_MAX) return XIP_ERR_SIZE;
    int slot = xip_pool_acquire(p, 0);
    if (slot < 0) {
        portENTER_CRITICAL(&registry_lock);
        l = link_at_locked((uint32_t)addr);
        if (l) l->dropped++;
        portEXIT_CRITICAL(&registry_lock);
        return XIP_ERR_NOMEM;
    }
    if (len) memcpy(xip_pool_slot(p, slot) + XIP_PKT_HEADROOM, data, len);
    link_fanout((uint32_t)addr, 0, slot, XIP_PKT_HEADROOM, (uint16_t)len, source, signal, time_us);
    return XIP_OK;
}

/* Release what a module still holds or has waiting (it ended): on the worker, in finish(). */
static unsigned packets_release(struct xip_instance *in)
{
    unsigned leaked = 0;
    if (!pool) return 0;
    for (unsigned i = 0; i < XIP_HELD_MAX; i++) {
        if (in->held.e[i].state == XIP_HELD_FREE) continue;
        if (in->held.e[i].state == XIP_HELD_OWNED) { in->stats.pkt_leaked++; leaked++; }
        drop_slot(in->held.e[i].slot);
        in->held.e[i].state = XIP_HELD_FREE;
    }
    portENTER_CRITICAL(&registry_lock);
    unsigned n = in->pkt_count;
    int slots[CONFIG_XIP_PKT_INBOX];
    for (unsigned k = 0; k < n; k++) slots[k] = in->pkt_in[(in->pkt_head + k) % CONFIG_XIP_PKT_INBOX].slot;
    in->pkt_count = 0;
    portEXIT_CRITICAL(&registry_lock);
    for (unsigned k = 0; k < n; k++) { drop_slot(slots[k]); in->stats.pkt_dropped++; }
    return leaked;
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
    xip_info_release(in->id);
    socket_release(in->id);
    flash_release(in->id);
    serial_release(in);
    links_release(in);
    const unsigned leaked_packets = packets_release(in);
    const uint32_t leaked_bytes = xip_mem_used(&in->mem);        /* cannot be freed: nobody knows where it is */
    in->stats.mem_leaked = leaked_bytes;
    if (leaked_packets || leaked_bytes) {
        ESP_LOGW(TAG, "%s ended holding %u packets (freed) and %u bytes of memory%s", in->name, leaked_packets,
                 (unsigned)leaked_bytes, leaked_bytes ? ": it needs a reboot to be replaced" : "");
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
}

static int dispatch_x(struct xip_instance *in, const xip_call *call)
{
    const int64_t t0 = esp_timer_get_time();
    int rc = in->handler(&in->table, in->state, call);
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
                           uint32_t from, uint64_t id, uint32_t flags, int32_t value, uint32_t time_us)
{
    const xip_call call = {.size = sizeof(call), .component = component, .method = method, .value = value, .data = data, .len = len,
                              .pkt = pkt, .from = from, .id = id, .flags = flags, .time_us = time_us};
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
                                   wanted ? 1u : 0u, 0, 0);
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
static void handle_packets(struct xip_instance *in)
{
    for (;;) {
        int slot = -1;
        uint16_t start = 0, len = 0;
        int32_t component = 0, method = 0;
        uint32_t from = 0;
        uint64_t id = 0;
        int32_t value = 0;
        uint32_t lflags = 0, time_us = 0;
        bool was_full = false;
        portENTER_CRITICAL(&registry_lock);
        if (in->pkt_count) {
            was_full = in->pkt_count >= CONFIG_XIP_PKT_INBOX;
            unsigned at = in->pkt_head;
            slot = in->pkt_in[at].slot; start = in->pkt_in[at].start; len = in->pkt_in[at].len;
            component = in->pkt_in[at].component; method = in->pkt_in[at].method;
            from = in->pkt_in[at].from; id = in->pkt_in[at].id;
            value = in->pkt_in[at].value; lflags = in->pkt_in[at].lflags; time_us = in->pkt_in[at].time_us;
            in->pkt_head = (uint8_t)((at + 1) % CONFIG_XIP_PKT_INBOX);
            in->pkt_count--;
        }
        portEXIT_CRITICAL(&registry_lock);
        if (slot < 0) return;
        if (was_full && hooks.inbox_ready) hooks.inbox_ready(hooks.packet_ctx, in->component);
        uint32_t h = xip_held_add(&in->held, slot, start, len, XIP_HELD_BORROWED);
        if (!h) { drop_slot(slot); in->stats.pkt_dropped++; continue; }
        in->stats.pkt_received++;
        const bool wants = id != 0 && !(lflags & XIP_CALL_LINK);      /* a frame of a link has an address token in `id`, not a request to answer */
        const int rc = dispatch_packet(in, component, method, xip_pool_slot(pool, slot) + start, len, h, from, id,
                                       (wants ? 1u : 0u) | lflags, value, time_us);
        xip_held_entry *e = xip_held_get(&in->held, h);
        if (e && e->state == XIP_HELD_BORROWED) {         /* not kept, not sent on */
            xip_held_take(&in->held, h);
            /* A provider returns the sole TX lease through completion; other
             * borrowed calls simply release their receive reference. */
            if ((lflags & XIP_CALL_LINK) && !(lflags & XIP_CALL_LINK_EVENT) && from < XIP_ADDR_LINK) link_tx_returned(from, slot);
            else drop_slot(slot);
        }
        if (settle(in, rc)) return;
    }
}

static void worker(void *arg)
{
    (void)arg;
    xip_event_t ev;
    for (;;) {
        if (atomic_exchange(&wifi_pending, false)) {
            ev = (xip_event_t){.kind = XIP_EV_WIFI, .value = (int32_t)atomic_load(&wifi_generation)};
        } else {
            if (xQueueReceive(worker_queue, &ev, portMAX_DELAY) != pdTRUE) continue;
            if (ev.kind == XIP_EV_WIFI) {
                if (!atomic_exchange(&wifi_pending, false)) continue;
                ev.value = (int32_t)atomic_load(&wifi_generation);
            }
        }
        if (ev.kind == XIP_EV_SLEEP) { xip_power_cycle(); continue; }
        if (ev.kind == XIP_EV_NETWORK || ev.kind == XIP_EV_WAKE || ev.kind == XIP_EV_WIFI) {
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
                    if (settle(in, dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_WAKE, ev.value, 0, NULL, 0))) continue;
                } else {
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
            continue;
        }
        struct xip_instance *in = find(ev.inst);
        if (!in) continue;   /* ended since the event was queued */
        switch (ev.kind) {
        case XIP_EV_START:
            settle(in, dispatch(in, XIP_HOST_COMPONENT, XIP_HOST_START, 0, 0, in->start_request, in->start_len));
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
        case XIP_EV_PACKET: handle_packets(in); break;
        case XIP_EV_LINK: {
            xip_call call = {.size = sizeof(call), .component = ev.component, .method = ev.method, .value = ev.value, .from = ev.from,
                                .id = ev.id, .flags = ev.flags};
            (void)settle(in, dispatch_x(in, &call));
            break;
        }
        case XIP_EV_IO: {
            if (ev.value >= XIP_SOCKET_EVENT_OPEN && ev.value <= XIP_SOCKET_EVENT_WRITE_DONE) socket_delivered((uint32_t)ev.id);
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
    serial_link = xip_link_native_register("serial", 256, XIP_LINK_FLAG_STREAM, serial_tx, NULL);
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
        h.module_manifest = ext->module_manifest;
        h.wifi_reconcile = ext->wifi_reconcile;
        h.wifi_tuning_set = ext->wifi_tuning_set; h.wifi_tuning_get = ext->wifi_tuning_get;
        h.packet = ext->packet; h.inbox_ready = ext->inbox_ready;
    }
    return xip_host_init(&h);
}

void xip_host_set_connected(bool connected)
{
    if (!inited || net_connected == connected) return;
    net_connected = connected;
    xip_event_t ev = {.kind = XIP_EV_NETWORK, .value = connected};
    (void)xQueueSend(worker_queue, &ev, 0);
}

void xip_power_post(void)
{
    if (!inited) return;
    xip_event_t ev = {.kind = XIP_EV_SLEEP};
    (void)xQueueSend(worker_queue, &ev, 0);
}

void xip_host_wake_window_value(int32_t value)
{
    if (!inited) return;
    xip_event_t ev = {.kind = XIP_EV_WAKE, .value = value};
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
    struct xip_instance *in = calloc(1, sizeof(*in));
    if (!in) return NULL;
    strlcpy(in->name, name, sizeof(in->name));
    in->handler = cfg->handler; in->done = cfg->done; in->done_ctx = cfg->done_ctx;
    in->state_bytes = cfg->state_bytes;
    in->component = cfg->component;
    in->port = cfg->port;
    in->stream = cfg->stream;
    xip_held_init(&in->held, CONFIG_XIP_PKT_HELD);
    xip_mem_init(&in->mem, CONFIG_XIP_MEM_QUOTA ? CONFIG_XIP_MEM_QUOTA : UINT32_MAX);
    in->state = calloc(1, cfg->state_bytes ? cfg->state_bytes : 1);
    in->inbox = malloc(CONFIG_XIP_REQUEST_MAX);
    if (!in->state || !in->inbox) { free(in->state); free(in->inbox); free(in); return NULL; }
    in->table = (xip_host){
        .device_info = h_device_info, .info_publish = h_info_publish, .get_component = h_get_component,
        .abi_version = XIP_ABI_VERSION,
        .size = sizeof(xip_host),
        .user = in,
        .log = h_log, .now_ms = h_now_ms,
        .get_setting = h_get_setting, .set_setting = h_set_setting,
        .alarm_set = h_alarm_set, .alarm_cancel = h_alarm_cancel,
        .modules_info = h_modules_info, .module_start = h_module_start, .module_stop = h_module_stop,
        .reset = h_reset,
        .pkt_alloc = h_pkt_alloc, .pkt_buf = h_pkt_buf, .pkt_keep = h_pkt_keep, .pkt_free = h_pkt_free, .send = h_send,
        .pkt_info = h_pkt_info, .mem_alloc = h_mem_alloc, .mem_free = h_mem_free, .pkt_span = h_pkt_span,
        .socket_read = h_socket_read, .socket_write = h_socket_write,
        .socket_close = h_socket_close, .socket_peer = h_socket_peer, .socket_write_pkt = h_socket_write_pkt,
        .flash_erase = h_flash_erase, .flash_write = h_flash_write, .flash_read = h_flash_read, .module_manifest = h_module_manifest, .nv = h_nv, .heap_info = h_heap_info, .heap_dump = h_heap_dump,
    };
    for (int i = 0; i < XIP_IRQ_SLOTS; i++) in->irq[i].inst = in;
    portENTER_CRITICAL(&registry_lock);
    int slot = -1;
    for (int i = 0; i < XIP_MAX_INSTANCES && slot < 0; i++) if (!registry[i]) slot = i;
    if (slot >= 0) { in->slot = slot; in->id = next_id++; registry[slot] = in; }
    portEXIT_CRITICAL(&registry_lock);
    if (slot < 0) { free(in->state); free(in->inbox); free(in); return NULL; }
    if (in->port) {
        int rc = port_bind(in);
        if (rc) ESP_LOGW(TAG, "%s: port %u not bound: %d", in->name, (unsigned)in->port, rc);
    }
    if (in->stream) {
        /* Declarative accept: the stream port listens on TCP port XIP_PORT_NET + stream, and each connection arrives as a socket event
         * (component, method 0, value SOCKET_EVENT_*, id = the handle) that the module reads and writes with socket_read / socket_write. */
        uint32_t listener;
        int rc = h_tcp_accept(in, (uint16_t)(XIP_PORT_NET + in->stream), XIP_SOCKET_MODE_DIRECT, in->component, 0, &listener);
        if (rc) ESP_LOGW(TAG, "%s: stream port %u not listening: %d", in->name, (unsigned)in->stream, rc);
    }
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
