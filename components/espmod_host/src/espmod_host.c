/* SPDX-License-Identifier: Apache-2.0 */
/* The event-driven host (abi/API.md, Model): one worker task calls the handler of every module for each event
 * (start, stop, request, interrupt, alarm). Instance lifecycle, alarms, and the core calls of the host table: log,
 * alloc, time, respond, emit, services, settings. Peripherals are in espmod_periph.c. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "esp_attr.h"
#include "nvs.h"
#include "internal.h"

static const char *TAG = "espmod";
static espmod_host_hooks_t hooks;
static bool inited;
static volatile bool net_connected;
static QueueHandle_t worker_queue;
static TaskHandle_t worker_task;
static struct espmod_instance *registry[EM_MAX_INSTANCES];
static uint32_t next_id = 1;
static portMUX_TYPE registry_lock = portMUX_INITIALIZER_UNLOCKED;
static em_alarm_ctx_t alarm_ctx[EM_MAX_INSTANCES * EM_ALARMS];

#define SETTING_KEY_MAX 15
#define SETTING_VALUE_MAX 256

static struct espmod_instance *I(void *user) { return user; }
static int dispatch(struct espmod_instance *in, int component, int method, int value, uint32_t flags, const uint8_t *data, size_t len);
static int dispatch_packet(struct espmod_instance *in, int component, int method, const uint8_t *data, size_t len, uint32_t pkt,
                           uint32_t from, uint64_t id, uint32_t flags);
static void serial_release(struct espmod_instance *in);
static struct espmod_instance *by_component(int component, const struct espmod_instance *except);
static bool known_locked(const struct espmod_instance *in);

static int h_log(void *user, const uint8_t *data, size_t len)
{
    struct espmod_instance *in = I(user);
    if (!data) return ESPMOD_ERR_ARG;
    char line[97];
    size_t n = len < sizeof(line) - 1 ? len : sizeof(line) - 1;
    memcpy(line, data, n);
    line[n] = 0;
    in->stats.logged++;
    /* WARN level: INFO is usually compiled out of a production Main, and a module's log is its only
     * way to explain a failure. */
    ESP_LOGW(TAG, "%s: %s%s", in->name, line, n == len ? "" : "...");
    return n == len ? ESPMOD_OK : ESPMOD_ERR_SIZE;
}

static uint64_t h_now_ms(void *user) { (void)user; return (uint64_t)(esp_timer_get_time() / 1000); }

static int h_respond(void *user, const uint8_t *data, size_t len)
{
    struct espmod_instance *in = I(user);
    if (!in->in_request) return ESPMOD_ERR_ARG;
    if (in->responded) return ESPMOD_ERR_BUSY;
    if ((len && !data) || len > CONFIG_ESPMOD_RESPONSE_MAX) return ESPMOD_ERR_SIZE;
    if (len) memcpy(in->response, data, len);
    in->response_len = len;
    in->responded = true;
    return ESPMOD_OK;
}

static int h_emit(void *user, uint16_t event_id, uint8_t value_type, const uint8_t *value, size_t len)
{
    struct espmod_instance *in = I(user);
    if (len && !value) return ESPMOD_ERR_ARG;
    in->stats.emitted++;
    em_events_add(in->name, event_id, value_type, value, len);
    if (!hooks.emit) return ESPMOD_OK;
    return hooks.emit(hooks.emit_ctx, in->name, event_id, value_type, value, len);
}

static int h_wifi_start(void *user, const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len)
{
    (void)user;
    if (!hooks.wifi_start) return ESPMOD_ERR_UNSUPPORTED;
    if ((ssid_len && !ssid) || (psk_len && !psk) || ssid_len > 32 || psk_len > 63) return ESPMOD_ERR_ARG;
    return hooks.wifi_start(ssid, ssid_len, psk, psk_len);
}

static int h_wifi_config(void *user, const uint8_t *ssid, size_t ssid_len, const uint8_t *psk, size_t psk_len)
{
    (void)user;
    if (!hooks.wifi_config) return ESPMOD_ERR_UNSUPPORTED;
    if ((ssid_len && !ssid) || (psk_len && !psk) || ssid_len > 32 || psk_len > 63) return ESPMOD_ERR_ARG;
    int rc = hooks.wifi_config(ssid, ssid_len, psk, psk_len);
    return rc == 0 ? 0 : rc == -1 ? ESPMOD_ERR_ARG : ESPMOD_ERR_IO;
}

static int h_wifi_stop(void *user)
{
    (void)user;
    if (!hooks.wifi_stop) return ESPMOD_ERR_UNSUPPORTED;
    hooks.wifi_stop();
    return 0;
}

static int h_wifi_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.wifi_status) return ESPMOD_ERR_UNSUPPORTED;
    if (!out || !capacity) return ESPMOD_ERR_ARG;
    return em_wifi_info(&hooks, out, capacity);
}

static int h_heap_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!out || !capacity) return ESPMOD_ERR_ARG;
    return em_heap_info(out, capacity);
}

static int h_nv(void *user, const uint8_t *key, size_t key_len, const uint8_t *value, size_t value_len, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.nv) return ESPMOD_ERR_UNSUPPORTED;
    if (!key || !key_len || key_len > 32 || (value_len && !value) || (!value && (!out || !capacity))) return ESPMOD_ERR_ARG;
    int rc = hooks.nv((const char *)key, key_len, value, value_len, out, capacity);
    return rc >= 0 ? rc : rc == -1 ? ESPMOD_ERR_ARG : rc == -3 ? ESPMOD_ERR_IO : rc == -5 ? ESPMOD_ERR_SIZE : ESPMOD_ERR_NOTFOUND;
}

static int h_heap_dump(void *user, int kind) { (void)user; return em_heap_dump(kind); }

static int h_wifi_scan(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.wifi_scan) return ESPMOD_ERR_UNSUPPORTED;
    if (!out || !capacity) return ESPMOD_ERR_ARG;
    return em_wifi_scan(&hooks, out, capacity);
}

static int h_loader_start(void *user, const uint8_t *name, size_t name_len)
{
    (void)user;
    if (!hooks.loader_start) return ESPMOD_ERR_UNSUPPORTED;
    if (!name || !name_len || name_len > 32) return ESPMOD_ERR_ARG;
    return hooks.loader_start(name, name_len);
}

static int h_loader_stop(void *user)
{
    (void)user;
    if (!hooks.loader_stop) return ESPMOD_ERR_UNSUPPORTED;
    hooks.loader_stop();
    return 0;
}

static int h_call_service(void *user, uint16_t tag, const uint8_t *req, size_t req_len,
                          uint8_t *resp, size_t resp_cap, size_t *resp_len, uint32_t timeout_ms)
{
    struct espmod_instance *in = I(user);
    if (resp_len) *resp_len = 0;
    if (!hooks.call_service) return ESPMOD_ERR_NOTFOUND;
    if (timeout_ms == 0 || timeout_ms > 250) timeout_ms = 250; /* a module must not stall the application */
    return hooks.call_service(hooks.service_ctx, in->name, tag, req, req_len, resp, resp_cap, resp_len, timeout_ms);
}

static int open_ns(struct espmod_instance *in, nvs_open_mode_t mode, nvs_handle_t *h)
{
    char ns[NVS_NS_NAME_MAX_SIZE];
    snprintf(ns, sizeof(ns), "em_%.11s", in->name);
    esp_err_t e = nvs_open(ns, mode, h);
    if (e == ESP_ERR_NVS_NOT_FOUND) return ESPMOD_ERR_NOTFOUND;
    return e == ESP_OK ? 0 : ESPMOD_ERR_IO;
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
    if (!key_of(key, key_len, k) || !value_len || (cap && !value)) return ESPMOD_ERR_ARG;
    nvs_handle_t h;
    int rc = open_ns(I(user), NVS_READONLY, &h);
    if (rc) return rc;
    size_t n = cap;
    esp_err_t e = nvs_get_blob(h, k, value, &n);
    if (e == ESP_ERR_NVS_INVALID_LENGTH) { /* ask the real size */
        e = nvs_get_blob(h, k, NULL, &n);
        *value_len = n;
        nvs_close(h);
        return e == ESP_OK ? ESPMOD_ERR_SIZE : ESPMOD_ERR_IO;
    }
    nvs_close(h);
    if (e == ESP_ERR_NVS_NOT_FOUND) return ESPMOD_ERR_NOTFOUND;
    if (e != ESP_OK) return ESPMOD_ERR_IO;
    *value_len = n;
    return 0;
}

static int h_set_setting(void *user, const uint8_t *key, size_t key_len, const uint8_t *value, size_t value_len)
{
    char k[SETTING_KEY_MAX + 1];
    if (!key_of(key, key_len, k) || (value_len && !value) || value_len > SETTING_VALUE_MAX) return ESPMOD_ERR_ARG;
    nvs_handle_t h;
    int rc = open_ns(I(user), NVS_READWRITE, &h);
    if (rc) return rc;
    esp_err_t e = value_len ? nvs_set_blob(h, k, value, value_len) : nvs_erase_key(h, k);
    if (e == ESP_ERR_NVS_NOT_FOUND && !value_len) e = ESP_OK;
    if (e == ESP_OK) e = nvs_commit(h);
    nvs_close(h);
    return e == ESP_OK ? 0 : ESPMOD_ERR_IO;
}


/* ---- alarms: the timers only queue events; the alarm state belongs to the worker ---- */
static uint32_t fire_delay_ms(uint32_t delay, uint32_t window, uint32_t flags)
{
    if (!(flags & ESPMOD_ALARM_FLEXIBLE) || !hooks.next_window_ms) return delay;
    uint32_t w = hooks.next_window_ms(hooks.window_ctx);
    if (w == 0) return delay;
    if (w >= delay && w - delay <= window) return w;   /* the next window is inside the allowed range */
    return w < delay ? delay : delay + window;         /* none inside it: as late as allowed, batching with others */
}

static void alarm_cb(void *arg)
{
    const em_alarm_ctx_t *c = arg;
    em_event_t ev = {.kind = EM_EV_ALARM, .inst = c->inst, .seq = c->seq, .alarm = c->index};
    (void)xQueueSend(worker_queue, &ev, 0);
}

static void alarm_arm(struct espmod_instance *in, int idx, uint32_t after_ms)
{
    em_alarm_t *a = &in->alarms[idx];
    em_alarm_ctx_t *ctx = &alarm_ctx[in->slot * EM_ALARMS + idx];
    esp_timer_stop(a->timer);
    a->seq++;
    ctx->inst = in->id; ctx->seq = a->seq; ctx->index = (uint8_t)idx;
    if (esp_timer_start_once(a->timer, (uint64_t)after_ms * 1000) != ESP_OK) a->used = false;
}

static int h_alarm_set(void *user, uint32_t delay_ms, uint32_t window_ms, uint32_t interval_ms, uint32_t flags,
                       int component, int method)
{
    struct espmod_instance *in = I(user);
    if (flags & ~(uint32_t)(ESPMOD_ALARM_FLEXIBLE | ESPMOD_ALARM_CONNECTED | ESPMOD_ALARM_WINDOW)) return ESPMOD_ERR_ARG;
    int idx = -1;
    for (int i = 0; i < EM_ALARMS && idx < 0; i++) if (!in->alarms[i].used) idx = i;
    if (idx < 0) return ESPMOD_ERR_NOMEM;
    em_alarm_t *a = &in->alarms[idx];
    if (!a->timer) {
        const esp_timer_create_args_t ta = {.callback = alarm_cb, .arg = &alarm_ctx[in->slot * EM_ALARMS + idx], .name = "em_alarm"};
        if (esp_timer_create(&ta, &a->timer) != ESP_OK) return ESPMOD_ERR_NOMEM;
    }
    a->used = true; a->waiting_net = false;
    a->window_ms = window_ms; a->interval_ms = interval_ms; a->flags = flags;
    a->component = component; a->method = method;
    a->due_us = esp_timer_get_time() + (int64_t)delay_ms * 1000;
    alarm_arm(in, idx, fire_delay_ms(delay_ms, window_ms, flags));
    return a->used ? idx : ESPMOD_ERR_IO;
}

static int h_alarm_cancel(void *user, int alarm)
{
    struct espmod_instance *in = I(user);
    if (alarm < 0 || alarm >= EM_ALARMS || !in->alarms[alarm].used) return ESPMOD_ERR_NOTFOUND;
    esp_timer_stop(in->alarms[alarm].timer);
    in->alarms[alarm].used = false;
    in->alarms[alarm].seq++;
    return ESPMOD_OK;
}

static int h_connected(void *user) { (void)user; return net_connected ? 1 : 0; }

/* ---- the console's calls: other modules, the serial port, the module list, reset ---- */
static struct espmod_instance *serial_owner;
static int serial_component, serial_method;

static void serial_close_task(void *arg)
{
    (void)arg;
    espmod_serial_stop();
    vTaskDelete(NULL);
}

/* The port goes back in the background: closing waits for the line task, which may be waiting for the worker. */
static void serial_release(struct espmod_instance *in)
{
    portENTER_CRITICAL(&registry_lock);
    const bool mine = serial_owner == in;
    if (mine) serial_owner = NULL;
    portEXIT_CRITICAL(&registry_lock);
    if (mine && xTaskCreate(serial_close_task, "em_serial_close", 3072, NULL, 4, NULL) != pdPASS) ESP_LOGE(TAG, "serial not closed");
}

/* A line arrived (serial task): it is a packet for the owner, who writes what it has to say with serial_write. */
static void on_serial_line(void *ctx, char *line, size_t len)
{
    (void)ctx;
    portENTER_CRITICAL(&registry_lock);
    struct espmod_instance *owner = serial_owner;
    const int component = serial_component, method = serial_method;
    portEXIT_CRITICAL(&registry_lock);
    if (owner && espmod_instance_send(owner, component, method, 0, (const uint8_t *)line, len) != 0)
        espmod_serial_write("error: busy\n", 12);
}

static int h_serial_open(void *user, int component, int method)
{
    struct espmod_instance *in = I(user);
    portENTER_CRITICAL(&registry_lock);
    const bool busy = serial_owner != NULL;
    if (!busy) { serial_owner = in; serial_component = component; serial_method = method; }
    portEXIT_CRITICAL(&registry_lock);
    if (busy) return ESPMOD_ERR_BUSY;
    int rc = espmod_serial_start(on_serial_line, NULL);
    if (rc) { portENTER_CRITICAL(&registry_lock); serial_owner = NULL; portEXIT_CRITICAL(&registry_lock); }
    return rc;
}

static int h_serial_write(void *user, const uint8_t *data, size_t len)
{
    if (serial_owner != I(user)) return ESPMOD_ERR_NOTFOUND;
    if (len && !data) return ESPMOD_ERR_ARG;
    return espmod_serial_write(data, len);
}

static int h_serial_close(void *user)
{
    serial_release(I(user));
    return ESPMOD_OK;
}

static struct espmod_instance *by_component(int component, const struct espmod_instance *except)
{
    struct espmod_instance *found = NULL;
    portENTER_CRITICAL(&registry_lock);
    for (int i = 0; i < EM_MAX_INSTANCES && !found; i++)
        if (registry[i] && registry[i] != except && registry[i]->component == component) found = registry[i];
    portEXIT_CRITICAL(&registry_lock);
    return found;
}

static int h_modules_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.modules_info) return ESPMOD_ERR_UNSUPPORTED;
    if (!out || !capacity) return ESPMOD_ERR_ARG;
    return hooks.modules_info(out, capacity);
}

static int h_module_start(void *user, const uint8_t *name, size_t name_len, const uint8_t *request, size_t request_len)
{
    (void)user;
    if (!hooks.module_start) return ESPMOD_ERR_UNSUPPORTED;
    if (!name || !name_len || (request_len && !request)) return ESPMOD_ERR_ARG;
    return hooks.module_start(name, name_len, request, request_len);
}

static int h_module_stop(void *user, const uint8_t *name, size_t name_len)
{
    (void)user;
    if (!hooks.module_stop) return ESPMOD_ERR_UNSUPPORTED;
    if (!name || !name_len) return ESPMOD_ERR_ARG;
    return hooks.module_stop(name, name_len);
}

static int h_module_manifest(void *user, const uint8_t *name, size_t name_len, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!hooks.module_manifest) return ESPMOD_ERR_UNSUPPORTED;
    if (!name || !name_len || (capacity && !out)) return ESPMOD_ERR_ARG;
    return hooks.module_manifest(name, name_len, out, capacity);
}

static int h_events_info(void *user, uint8_t *out, size_t capacity)
{
    (void)user;
    if (!out || !capacity) return ESPMOD_ERR_ARG;
    return em_events_info(out, capacity);
}

static void restart_later(void *arg) { (void)arg; esp_restart(); }

static int h_reset(void *user)
{
    (void)user;
    esp_timer_handle_t t;
    const esp_timer_create_args_t a = {.callback = restart_later, .name = "restart"};
    if (esp_timer_create(&a, &t) != ESP_OK || esp_timer_start_once(t, 300000) != ESP_OK) return ESPMOD_ERR_NOMEM;
    return ESPMOD_OK;
}

/* ---- the managed heap: a module frees what it took before it stops, and the host checks the balance ---- */
static void *h_mem_alloc(void *user, size_t size) { return espmod_mem_alloc(&I(user)->mem, size); }
static int h_mem_free(void *user, void *ptr) { return espmod_mem_free(&I(user)->mem, ptr) == 0 ? ESPMOD_OK : ESPMOD_ERR_ARG; }

/* ---- packets (abi/API.md, PKT): the C pool, and what each module holds ---- */
static espmod_pool *pool;
static SemaphoreHandle_t pool_lock;

static size_t pool_free_heap(void) { return heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT); }

/* The pool is allocated when the first packet is wanted: a device whose modules never use one pays nothing. */
static espmod_pool *pool_get(void)
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
            if (hooks.pool->slot_size(hooks.pool->ctx) >= ESPMOD_PKT_SLOT_MIN) pool = espmod_pool_external(hooks.pool);
            else ESP_LOGE(TAG, "application pool slots are too small (%u < %u)", (unsigned)hooks.pool->slot_size(hooks.pool->ctx), (unsigned)ESPMOD_PKT_SLOT_MIN);
        } else {
            pool = espmod_pool_create(CONFIG_ESPMOD_PKT_SLOTS, ESPMOD_PKT_SLOT_MIN);
            espmod_pool_set_policy(pool, CONFIG_ESPMOD_PKT_KEEP, pool_free_heap, CONFIG_ESPMOD_PKT_HEAP_RESERVE);
        }
    }
    xSemaphoreGive(pool_lock);
    return pool;
}

#define PKT_TAIL (ESPMOD_PKT_SLOT_MIN - ESPMOD_PKT_HEADROOM - ESPMOD_PKT_MAX)

static void drop_slot(int slot) { espmod_pool_release(pool, slot); }

static int h_pkt_alloc(void *user, uint32_t *out)
{
    struct espmod_instance *in = I(user);
    espmod_pool *p = pool_get();
    if (!out) return ESPMOD_ERR_ARG;
    *out = 0;
    if (!p) return ESPMOD_ERR_NOMEM;
    if (espmod_held_count(&in->held, ESPMOD_HELD_OWNED) >= in->held.quota) return ESPMOD_ERR_BUSY;
    int slot = espmod_pool_acquire(p, CONFIG_ESPMOD_PKT_RESERVE);
    if (slot < 0) return ESPMOD_ERR_NOMEM;
    uint32_t h = espmod_held_add(&in->held, slot, ESPMOD_PKT_HEADROOM, 0, ESPMOD_HELD_OWNED);
    if (!h) { drop_slot(slot); return ESPMOD_ERR_BUSY; }
    *out = h;
    return ESPMOD_OK;
}

static int h_pkt_buf(void *user, uint32_t pkt, uint8_t **ptr, size_t *max)
{
    struct espmod_instance *in = I(user);
    espmod_held_entry *e = espmod_held_get(&in->held, pkt);
    if (!e || !ptr || !max) return ESPMOD_ERR_ARG;
    if (espmod_pool_refs(pool, e->slot) != 1) return ESPMOD_ERR_BUSY;     /* shared: something else still reads it */
    *ptr = espmod_pool_slot(pool, e->slot) + e->start;
    *max = espmod_pool_slot_size(pool) - e->start - PKT_TAIL;
    if (*max > ESPMOD_PKT_MAX) *max = ESPMOD_PKT_MAX;
    return ESPMOD_OK;
}

static int h_pkt_span(void *user, uint32_t pkt, uint8_t **base, size_t *capacity)
{
    struct espmod_instance *in = I(user);
    espmod_held_entry *e = espmod_held_get(&in->held, pkt);
    if (!e || !base || !capacity || !pool) return ESPMOD_ERR_ARG;
    if (espmod_pool_refs(pool, e->slot) != 1) return ESPMOD_ERR_BUSY;
    *base = espmod_pool_slot(pool, e->slot);
    *capacity = espmod_pool_slot_size(pool);
    return ESPMOD_OK;
}

/* The socket and asynchronous flash table entries are present for ABI size
 * compatibility. Until the C event bridge is installed they report the
 * feature as unavailable instead of leaving null function pointers. */
static int h_tcp_accept(void *u, uint16_t p, uint32_t m, int32_t c, int32_t f, uint32_t *l)
{ (void)u; (void)p; (void)m; (void)c; (void)f; if (l) *l = 0; return ESPMOD_ERR_UNSUPPORTED; }
static int h_socket_read(void *u, uint32_t s, uint8_t *b, size_t n)
{ (void)u; (void)s; (void)b; (void)n; return ESPMOD_ERR_UNSUPPORTED; }
static int h_socket_write(void *u, uint32_t s, const uint8_t *b, size_t n)
{ (void)u; (void)s; (void)b; (void)n; return ESPMOD_ERR_UNSUPPORTED; }
static int h_socket_close(void *u, uint32_t s)
{ (void)u; (void)s; return ESPMOD_ERR_UNSUPPORTED; }
static int h_socket_peer(void *u, uint32_t s, uint8_t *a, uint16_t *p, uint32_t *sc)
{ (void)u; (void)s; (void)a; (void)p; (void)sc; return ESPMOD_ERR_UNSUPPORTED; }
static int h_socket_write_pkt(void *u, uint32_t s, uint32_t p, uint32_t o, uint32_t n)
{ (void)u; (void)s; (void)p; (void)o; (void)n; return ESPMOD_ERR_UNSUPPORTED; }
static int h_flash_erase(void *u, uint32_t a, uint32_t n, int32_t c, int32_t m, uint64_t id)
{ (void)u; (void)a; (void)n; (void)c; (void)m; (void)id; return ESPMOD_ERR_UNSUPPORTED; }
static int h_flash_write(void *u, uint32_t a, const uint8_t *b, size_t n, int32_t c, int32_t m, uint64_t id)
{ (void)u; (void)a; (void)b; (void)n; (void)c; (void)m; (void)id; return ESPMOD_ERR_UNSUPPORTED; }
static int h_flash_read(void *u, uint32_t a, uint8_t *b, size_t n)
{ (void)u; (void)a; (void)b; (void)n; return ESPMOD_ERR_UNSUPPORTED; }

static int h_pkt_keep(void *user, uint32_t pkt)
{
    int rc = espmod_held_keep(&I(user)->held, pkt);
    return rc == 0 ? ESPMOD_OK : rc == -2 ? ESPMOD_ERR_BUSY : ESPMOD_ERR_ARG;
}

static int h_pkt_free(void *user, uint32_t pkt)
{
    espmod_held_entry e = espmod_held_take(&I(user)->held, pkt);
    if (e.slot < 0) return ESPMOD_ERR_ARG;
    drop_slot(e.slot);
    return ESPMOD_OK;
}

static int h_pkt_info(void *user, uint32_t *headroom, uint32_t *max)
{
    (void)user;
    if (!headroom || !max) return ESPMOD_ERR_ARG;
    *headroom = ESPMOD_PKT_HEADROOM;
    *max = ESPMOD_PKT_MAX;
    return ESPMOD_OK;
}

/* The packet goes to the target's inbox ring and a worker event; on success the target owns the reference. */
static int deliver(struct espmod_instance *t, int slot, uint16_t start, uint16_t len, int component, int method,
                   uint32_t from, uint64_t id)
{
    uint32_t tid = 0;
    int rc = ESPMOD_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(t)) {
        if (t->pkt_count >= CONFIG_ESPMOD_PKT_INBOX) rc = ESPMOD_ERR_BUSY;
        else {
            unsigned at = (t->pkt_head + t->pkt_count++) % CONFIG_ESPMOD_PKT_INBOX;
            t->pkt_in[at].slot = (int16_t)slot; t->pkt_in[at].start = start; t->pkt_in[at].len = len;
            t->pkt_in[at].component = component; t->pkt_in[at].method = method;
            t->pkt_in[at].from = from; t->pkt_in[at].id = id;
            tid = t->id;
            rc = 0;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if (rc) return rc;
    em_event_t ev = {.kind = EM_EV_PACKET, .inst = tid};
    if (xQueueSend(worker_queue, &ev, 0) == pdTRUE) return 0;
    portENTER_CRITICAL(&registry_lock);              /* no room for the event: take the packet back out */
    if (known_locked(t)) {
        for (unsigned k = 0; k < t->pkt_count; k++) {
            unsigned at = (t->pkt_head + k) % CONFIG_ESPMOD_PKT_INBOX;
            if (t->pkt_in[at].slot != slot) continue;
            for (unsigned j = k; j + 1 < t->pkt_count; j++)
                t->pkt_in[(t->pkt_head + j) % CONFIG_ESPMOD_PKT_INBOX] = t->pkt_in[(t->pkt_head + j + 1) % CONFIG_ESPMOD_PKT_INBOX];
            t->pkt_count--;
            break;
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    return ESPMOD_ERR_BUSY;
}

static int h_send(void *user, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t id)
{
    struct espmod_instance *in = I(user);
    espmod_held_entry e = espmod_held_take(&in->held, pkt);       /* consumed on success and on failure */
    if (e.slot < 0) return ESPMOD_ERR_ARG;
    int rc;
    if (len > ESPMOD_PKT_MAX || e.start + len > espmod_pool_slot_size(pool)) {
        rc = ESPMOD_ERR_SIZE;
    } else if (dst == ESPMOD_ADDR_MAIN) {
        rc = hooks.packet ? hooks.packet(hooks.packet_ctx, in->name, in->component, component, method, id,
                                         espmod_pool_slot(pool, e.slot) + e.start, len) : ESPMOD_ERR_NOTFOUND;
        drop_slot(e.slot);                                         /* lent to the hook for the call */
        if (rc == 0) in->stats.pkt_sent++;
        return rc;
    } else {
        struct espmod_instance *t = by_component((int)dst, NULL);
        rc = t ? deliver(t, e.slot, e.start, (uint16_t)len, component, method, in->component, id) : ESPMOD_ERR_NOTFOUND;
        if (rc == 0) { in->stats.pkt_sent++; return 0; }
    }
    in->stats.pkt_dropped++;
    drop_slot(e.slot);
    return rc;
}

int espmod_instance_send(espmod_instance_t *in, int component, int method, uint64_t id, const uint8_t *data, size_t len)
{
    espmod_pool *p = pool_get();
    if (!in || (len && !data) || !p) return in && p ? ESPMOD_ERR_ARG : ESPMOD_ERR_NOMEM;
    if (len > ESPMOD_PKT_MAX) return ESPMOD_ERR_SIZE;
    int slot = espmod_pool_acquire(p, 0);                         /* the application may use every packet */
    if (slot < 0) return ESPMOD_ERR_NOMEM;
    if (len) memcpy(espmod_pool_slot(p, slot) + ESPMOD_PKT_HEADROOM, data, len);
    int rc = deliver(in, slot, ESPMOD_PKT_HEADROOM, (uint16_t)len, component, method, ESPMOD_ADDR_MAIN, id);
    if (rc) { drop_slot(slot); in->stats.pkt_dropped++; }
    return rc;
}

/* The module used respond() on a packet that carried an id: the same bytes go to the sender as a message with that id. */
static void answer(struct espmod_instance *in, uint32_t to, int component, int method, uint64_t id, const uint8_t *data, size_t len)
{
    if (to == ESPMOD_ADDR_MAIN) {
        if (hooks.packet) hooks.packet(hooks.packet_ctx, in->name, in->component, component, method, id, data, len);
        return;
    }
    struct espmod_instance *t = by_component((int)to, NULL);
    espmod_pool *p = pool_get();
    if (!t || !p) return;
    int slot = espmod_pool_acquire(p, 0);
    if (slot < 0) { in->stats.pkt_dropped++; return; }
    if (len) memcpy(espmod_pool_slot(p, slot) + ESPMOD_PKT_HEADROOM, data, len);
    if (deliver(t, slot, ESPMOD_PKT_HEADROOM, (uint16_t)len, component, method, in->component, id)) { drop_slot(slot); in->stats.pkt_dropped++; }
    else in->stats.pkt_sent++;
}

/* Release what a module still holds or has waiting (it ended): on the worker, in finish(). */
static unsigned packets_release(struct espmod_instance *in)
{
    unsigned leaked = 0;
    if (!pool) return 0;
    for (unsigned i = 0; i < ESPMOD_HELD_MAX; i++) {
        if (in->held.e[i].state == ESPMOD_HELD_FREE) continue;
        if (in->held.e[i].state == ESPMOD_HELD_OWNED) { in->stats.pkt_leaked++; leaked++; }
        drop_slot(in->held.e[i].slot);
        in->held.e[i].state = ESPMOD_HELD_FREE;
    }
    portENTER_CRITICAL(&registry_lock);
    unsigned n = in->pkt_count;
    int slots[CONFIG_ESPMOD_PKT_INBOX];
    for (unsigned k = 0; k < n; k++) slots[k] = in->pkt_in[(in->pkt_head + k) % CONFIG_ESPMOD_PKT_INBOX].slot;
    in->pkt_count = 0;
    portEXIT_CRITICAL(&registry_lock);
    for (unsigned k = 0; k < n; k++) { drop_slot(slots[k]); in->stats.pkt_dropped++; }
    return leaked;
}

/* ---- instances ---- */
static struct espmod_instance *find_locked(uint32_t id)
{
    for (int i = 0; i < EM_MAX_INSTANCES; i++) if (registry[i] && registry[i]->id == id) return registry[i];
    return NULL;
}

static struct espmod_instance *find(uint32_t id)
{
    portENTER_CRITICAL(&registry_lock);
    struct espmod_instance *in = find_locked(id);
    portEXIT_CRITICAL(&registry_lock);
    return in;
}

static bool known_locked(const struct espmod_instance *in)
{
    for (int i = 0; i < EM_MAX_INSTANCES; i++) if (in && registry[i] == in) return true;
    return false;
}

/* End an instance: on the worker only. */
static void finish(struct espmod_instance *in, int result)
{
    portENTER_CRITICAL(&registry_lock);
    registry[in->slot] = NULL;
    SemaphoreHandle_t waiter = in->waiter_sem;
    in->waiter_sem = NULL;
    if (waiter) *in->waiter_rc = ESPMOD_ERR_NOTFOUND;
    portEXIT_CRITICAL(&registry_lock);
    if (waiter) xSemaphoreGive(waiter);
    for (int i = 0; i < EM_ALARMS; i++) {
        if (!in->alarms[i].timer) continue;
        esp_timer_stop(in->alarms[i].timer);
        esp_timer_delete(in->alarms[i].timer);
        alarm_ctx[in->slot * EM_ALARMS + i].seq++;   /* a callback already in flight is stale */
    }
    serial_release(in);
    const unsigned leaked_packets = packets_release(in);
    const uint32_t leaked_bytes = espmod_mem_used(&in->mem);        /* cannot be freed: nobody knows where it is */
    in->stats.mem_leaked = leaked_bytes;
    if (leaked_packets || leaked_bytes) {
        ESP_LOGW(TAG, "%s ended holding %u packets (freed) and %u bytes of memory%s", in->name, leaked_packets,
                 (unsigned)leaked_bytes, leaked_bytes ? ": it needs a reboot to be replaced" : "");
        if (result >= 0) result = ESPMOD_ERR_LEAK;
    }
    em_periph_release(in);
    espmod_done_fn done = in->done;
    void *ctx = in->done_ctx;
    espmod_stats_t stats = in->stats;
    ESP_LOGI(TAG, "%s ended: %d (%u calls, max %u us)", in->name, result, (unsigned)stats.calls, (unsigned)stats.max_handler_us);
    free(in->state); free(in->inbox); free(in->start_request); free(in->response);
    free(in);
    if (done) done(ctx, result, &stats);
}

static int dispatch_x(struct espmod_instance *in, const espmod_call *call)
{
    const int64_t t0 = esp_timer_get_time();
    int rc = in->handler(&in->table, in->state, call);
    uint32_t us = (uint32_t)(esp_timer_get_time() - t0);
    in->stats.calls++;
    if (us > in->stats.max_handler_us) in->stats.max_handler_us = us;
    if (us / 1000 > CONFIG_ESPMOD_HANDLER_MS) {
        in->stats.overruns++;
        ESP_LOGW(TAG, "%s: handler %d.%d took %u ms (budget %d)", in->name, call->component, call->method, (unsigned)(us / 1000),
                 CONFIG_ESPMOD_HANDLER_MS);
    }
    return rc;
}

static int dispatch(struct espmod_instance *in, int component, int method, int value, uint32_t flags,
                    const uint8_t *data, size_t len)
{
    const espmod_call call = {.size = sizeof(call), .component = component, .method = method, .value = value,
                              .flags = flags, .data = data, .len = len};
    return dispatch_x(in, &call);
}

static int dispatch_packet(struct espmod_instance *in, int component, int method, const uint8_t *data, size_t len, uint32_t pkt,
                           uint32_t from, uint64_t id, uint32_t flags)
{
    const espmod_call call = {.size = sizeof(call), .component = component, .method = method, .data = data, .len = len,
                              .pkt = pkt, .from = from, .id = id, .flags = flags};
    return dispatch_x(in, &call);
}

/* After a handler call: a negative result or DONE ends the instance. True when it ended. */
static bool settle(struct espmod_instance *in, int rc)
{
    if (rc < 0 || rc == ESPMOD_RET_DONE) {
        finish(in, rc);
        return true;
    }
    return false;
}

static void handle_alarm(struct espmod_instance *in, const em_event_t *ev)
{
    em_alarm_t *a = &in->alarms[ev->alarm];
    if (!a->used || a->seq != ev->seq) return;   /* cancelled or re-armed since */
    if ((a->flags & ESPMOD_ALARM_CONNECTED) && !net_connected) { a->waiting_net = true; return; }
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

static void handle_request(struct espmod_instance *in, const em_event_t *ev)
{
    if (!in->response) in->response = malloc(CONFIG_ESPMOD_RESPONSE_MAX);
    portENTER_CRITICAL(&registry_lock);
    const bool wanted = in->waiter_sem != NULL;
    portEXIT_CRITICAL(&registry_lock);
    in->in_request = true; in->responded = false; in->response_len = 0;
    const int rc = dispatch(in, ev->component, ev->method, 0, wanted ? 1 : 0, in->inbox, in->inbox_len);
    in->in_request = false;
    portENTER_CRITICAL(&registry_lock);
    in->inbox_full = false;
    SemaphoreHandle_t waiter = in->waiter_sem;
    in->waiter_sem = NULL;
    if (waiter) {
        size_t n = in->responded ? in->response_len : 0;
        if (n > in->waiter_cap) { n = in->waiter_cap; *in->waiter_rc = ESPMOD_ERR_SIZE; } else *in->waiter_rc = ESPMOD_OK;
        if (n) memcpy(in->waiter_buf, in->response, n);
        *in->waiter_len = n;
    }
    portEXIT_CRITICAL(&registry_lock);
    if (waiter) xSemaphoreGive(waiter);
    settle(in, rc);
}

/* Deliver what is waiting, one call each: the packet is lent for the call and goes back to the pool when the handler
 * returns, unless the module kept it. */
static void handle_packets(struct espmod_instance *in)
{
    for (;;) {
        int slot = -1;
        uint16_t start = 0, len = 0;
        int32_t component = 0, method = 0;
        uint32_t from = 0;
        uint64_t id = 0;
        bool was_full = false;
        portENTER_CRITICAL(&registry_lock);
        if (in->pkt_count) {
            was_full = in->pkt_count >= CONFIG_ESPMOD_PKT_INBOX;
            unsigned at = in->pkt_head;
            slot = in->pkt_in[at].slot; start = in->pkt_in[at].start; len = in->pkt_in[at].len;
            component = in->pkt_in[at].component; method = in->pkt_in[at].method;
            from = in->pkt_in[at].from; id = in->pkt_in[at].id;
            in->pkt_head = (uint8_t)((at + 1) % CONFIG_ESPMOD_PKT_INBOX);
            in->pkt_count--;
        }
        portEXIT_CRITICAL(&registry_lock);
        if (slot < 0) return;
        if (was_full && hooks.inbox_ready) hooks.inbox_ready(hooks.packet_ctx, in->component);
        uint32_t h = espmod_held_add(&in->held, slot, start, len, ESPMOD_HELD_BORROWED);
        if (!h) { drop_slot(slot); in->stats.pkt_dropped++; continue; }
        in->stats.pkt_received++;
        const bool wants = id != 0;
        if (wants && !in->response) in->response = malloc(CONFIG_ESPMOD_RESPONSE_MAX);
        in->in_request = wants && in->response; in->responded = false; in->response_len = 0;
        const int rc = dispatch_packet(in, component, method, espmod_pool_slot(pool, slot) + start, len, h, from, id, wants ? 1 : 0);
        in->in_request = false;
        if (wants && in->responded) answer(in, from, component, method, id, in->response, in->response_len);
        espmod_held_entry *e = espmod_held_get(&in->held, h);
        if (e && e->state == ESPMOD_HELD_BORROWED) {         /* not kept, not sent on */
            espmod_held_take(&in->held, h);
            drop_slot(slot);
        }
        if (settle(in, rc)) return;
    }
}

static void worker(void *arg)
{
    (void)arg;
    em_event_t ev;
    for (;;) {
        if (xQueueReceive(worker_queue, &ev, portMAX_DELAY) != pdTRUE) continue;
        if (ev.kind == EM_EV_NETWORK || ev.kind == EM_EV_WAKE) {
            for (int i = 0; i < EM_MAX_INSTANCES; i++) {
                struct espmod_instance *in = registry[i];
                if (!in) continue;
                if (ev.kind == EM_EV_WAKE) {
                    bool want = false;
                    for (int k = 0; k < EM_ALARMS; k++) want |= in->alarms[k].used && (in->alarms[k].flags & ESPMOD_ALARM_WINDOW);
                    if (!want) continue;
                    if (settle(in, dispatch(in, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_WAKE, 0, 0, NULL, 0))) continue;
                } else {
                    if (settle(in, dispatch(in, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_NETWORK, ev.value, 0, NULL, 0))) continue;
                    if (!ev.value) continue;
                    for (int k = 0; k < EM_ALARMS; k++) {
                        em_alarm_t *a = &in->alarms[k];
                        if (!a->used || !a->waiting_net) continue;
                        a->waiting_net = false;
                        a->due_us = esp_timer_get_time();
                        alarm_arm(in, k, fire_delay_ms(0, a->window_ms, a->flags));
                    }
                }
            }
            continue;
        }
        struct espmod_instance *in = find(ev.inst);
        if (!in) continue;   /* ended since the event was queued */
        switch (ev.kind) {
        case EM_EV_START:
            settle(in, dispatch(in, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_START, 0, 0, in->start_request, in->start_len));
            break;
        case EM_EV_CALL:
            if (ev.method == -1 && ev.component == ESPMOD_HOST_COMPONENT) {   /* stop request */
                dispatch(in, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_STOP, 0, 0, NULL, 0);
                finish(in, 0);
            } else {
                settle(in, dispatch(in, ev.component, ev.method, ev.value, 0, NULL, 0));
            }
            break;
        case EM_EV_REQUEST: handle_request(in, &ev); break;
        case EM_EV_PACKET: handle_packets(in); break;
        case EM_EV_ALARM: handle_alarm(in, &ev); break;
        default: break;
        }
    }
}

bool IRAM_ATTR em_post_isr(const em_event_t *ev, BaseType_t *higher) { return xQueueSendFromISR(worker_queue, ev, higher) == pdTRUE; }
bool em_connected(void) { return net_connected; }

uint32_t espmod_host_stack_high_water(void) { return worker_task ? (uint32_t)uxTaskGetStackHighWaterMark(worker_task) : 0; }

esp_err_t espmod_host_init(const espmod_host_hooks_t *h)
{
    if (h) hooks = *h;
    em_periph_reserve((h ? h->reserved_pins : 0) | em_serial_pins());
    if (inited) return ESP_OK;
    worker_queue = xQueueCreate(CONFIG_ESPMOD_EVENT_QUEUE, sizeof(em_event_t));
    if (!worker_queue) return ESP_ERR_NO_MEM;
    if (xTaskCreate(worker, "espmod", CONFIG_ESPMOD_WORKER_STACK, NULL, CONFIG_ESPMOD_WORKER_PRIORITY, &worker_task) != pdPASS)
        return ESP_ERR_NO_MEM;
    em_periph_init();
    inited = true;
    return ESP_OK;
}

static espmod_host_emit_fn simple_emit;

static int simple_on_emit(void *ctx, const char *module, uint16_t id, uint8_t type, const uint8_t *v, size_t len)
{
    (void)ctx;
    return simple_emit ? simple_emit(module, id, type, v, len) : 0;
}

esp_err_t espmod_host_init_simple(espmod_host_emit_fn emit, const espmod_host_ext_t *ext)
{
    simple_emit = emit;
    espmod_host_hooks_t h = {.emit = simple_on_emit};
    if (ext) {
        h.wifi_start = ext->wifi_start; h.wifi_stop = ext->wifi_stop; h.wifi_status = ext->wifi_status;
        h.wifi_config = ext->wifi_config; h.wifi_saved = ext->wifi_saved; h.nv = ext->nv; h.pool = ext->pool; h.wifi_scan = ext->wifi_scan;
        h.loader_start = ext->loader_start; h.loader_stop = ext->loader_stop;
        h.modules_info = ext->modules_info; h.module_start = ext->module_start; h.module_stop = ext->module_stop;
        h.module_manifest = ext->module_manifest;
        h.packet = ext->packet; h.inbox_ready = ext->inbox_ready;
    }
    return espmod_host_init(&h);
}

void espmod_host_set_connected(bool connected)
{
    if (!inited || net_connected == connected) return;
    net_connected = connected;
    em_event_t ev = {.kind = EM_EV_NETWORK, .value = connected};
    (void)xQueueSend(worker_queue, &ev, 0);
}

void espmod_host_wake_window(void)
{
    if (!inited) return;
    em_event_t ev = {.kind = EM_EV_WAKE};
    (void)xQueueSend(worker_queue, &ev, 0);
}

uint32_t espmod_host_features(void)
{
    return ESPMOD_FEAT_SETTINGS | ESPMOD_FEAT_SPI | ESPMOD_FEAT_SPI_READY | ESPMOD_FEAT_I2C |
           ESPMOD_FEAT_ADC | ESPMOD_FEAT_RGBLED | ESPMOD_FEAT_SERVICE |
           (hooks.next_window_ms ? ESPMOD_FEAT_ALARM_FLEX : 0) | (hooks.wifi_start ? ESPMOD_FEAT_WIFI : 0) |
           (hooks.loader_start ? ESPMOD_FEAT_LOADER : 0) |
           ESPMOD_FEAT_SERIAL | ESPMOD_FEAT_PACKETS | ESPMOD_FEAT_MEMORY | (hooks.modules_info ? ESPMOD_FEAT_MODULES : 0);
}

espmod_instance_t *espmod_instance_create(const char *name, const espmod_instance_config_t *cfg)
{
    if (!inited || !name || !*name || !cfg || !cfg->handler || cfg->state_bytes > 4096) return NULL;
    struct espmod_instance *in = calloc(1, sizeof(*in));
    if (!in) return NULL;
    strlcpy(in->name, name, sizeof(in->name));
    in->handler = cfg->handler; in->done = cfg->done; in->done_ctx = cfg->done_ctx;
    in->state_bytes = cfg->state_bytes;
    in->component = cfg->component;
    espmod_held_init(&in->held, CONFIG_ESPMOD_PKT_HELD);
    espmod_mem_init(&in->mem, CONFIG_ESPMOD_MEM_QUOTA);
    in->state = calloc(1, cfg->state_bytes ? cfg->state_bytes : 1);
    in->inbox = malloc(CONFIG_ESPMOD_REQUEST_MAX);
    if (!in->state || !in->inbox) { free(in->state); free(in->inbox); free(in); return NULL; }
    in->table = (espmod_host){
        .abi_version = ESPMOD_ABI_VERSION,
        .size = sizeof(espmod_host),
        .features = espmod_host_features(),
        .user = in,
        .log = h_log, .now_ms = h_now_ms, .respond = h_respond, .emit = h_emit,
        .call_service = h_call_service, .get_setting = h_get_setting, .set_setting = h_set_setting,
        .alarm_set = h_alarm_set, .alarm_cancel = h_alarm_cancel, .connected = h_connected,
        .wifi_start = h_wifi_start, .wifi_stop = h_wifi_stop, .wifi_info = h_wifi_info, .wifi_config = h_wifi_config,
        .loader_start = h_loader_start, .loader_stop = h_loader_stop,
        .serial_open = h_serial_open, .serial_write = h_serial_write, .serial_close = h_serial_close,
        .modules_info = h_modules_info, .module_start = h_module_start, .module_stop = h_module_stop,
        .events_info = h_events_info, .reset = h_reset,
        .pkt_alloc = h_pkt_alloc, .pkt_buf = h_pkt_buf, .pkt_keep = h_pkt_keep, .pkt_free = h_pkt_free, .send = h_send,
        .pkt_info = h_pkt_info, .mem_alloc = h_mem_alloc, .mem_free = h_mem_free, .pkt_span = h_pkt_span,
        .tcp_accept = h_tcp_accept, .socket_read = h_socket_read, .socket_write = h_socket_write,
        .socket_close = h_socket_close, .socket_peer = h_socket_peer, .socket_write_pkt = h_socket_write_pkt,
        .flash_erase = h_flash_erase, .flash_write = h_flash_write, .flash_read = h_flash_read, .module_manifest = h_module_manifest, .wifi_scan = h_wifi_scan, .nv = h_nv, .heap_info = h_heap_info, .heap_dump = h_heap_dump,
    };
    em_periph_fill(&in->table);
    for (int i = 0; i < EM_IRQ_SLOTS; i++) in->irq[i].inst = in;
    portENTER_CRITICAL(&registry_lock);
    int slot = -1;
    for (int i = 0; i < EM_MAX_INSTANCES && slot < 0; i++) if (!registry[i]) slot = i;
    if (slot >= 0) { in->slot = slot; in->id = next_id++; registry[slot] = in; }
    portEXIT_CRITICAL(&registry_lock);
    if (slot < 0) { free(in->state); free(in->inbox); free(in); return NULL; }
    return in;
}

const espmod_host *espmod_instance_host(const espmod_instance_t *inst) { return inst ? &inst->table : NULL; }

static int queue_event(struct espmod_instance *in, em_event_t *ev)
{
    ev->inst = in->id;
    return xQueueSend(worker_queue, ev, 0) == pdTRUE ? 0 : ESPMOD_ERR_BUSY;
}

int espmod_instance_start(espmod_instance_t *in, const uint8_t *data, size_t len)
{
    if (!in || (len && !data)) return ESPMOD_ERR_ARG;
    uint8_t *copy = len ? malloc(len) : NULL;
    if (len && !copy) return ESPMOD_ERR_NOMEM;
    if (len) memcpy(copy, data, len);
    int rc = ESPMOD_ERR_NOTFOUND;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in) && !in->start_request) { in->start_request = copy; in->start_len = len; rc = 0; copy = NULL; }
    portEXIT_CRITICAL(&registry_lock);
    free(copy);
    if (rc) return rc;
    em_event_t ev = {.kind = EM_EV_START};
    return queue_event(in, &ev);
}

int espmod_instance_stop(espmod_instance_t *in)
{
    if (!in) return ESPMOD_ERR_ARG;
    portENTER_CRITICAL(&registry_lock);
    const bool ok = known_locked(in);
    const uint32_t id = ok ? in->id : 0;
    portEXIT_CRITICAL(&registry_lock);
    if (!ok) return ESPMOD_ERR_NOTFOUND;
    em_event_t ev = {.kind = EM_EV_CALL, .inst = id, .component = ESPMOD_HOST_COMPONENT, .method = -1};
    return xQueueSend(worker_queue, &ev, 0) == pdTRUE ? 0 : ESPMOD_ERR_BUSY;
}

static int post_locked_copy(struct espmod_instance *in, int component, int method, const uint8_t *data, size_t len,
                            SemaphoreHandle_t sem, uint8_t *buf, size_t cap, size_t *out_len, int *rc_out)
{
    int rc = ESPMOD_ERR_NOTFOUND;
    uint32_t id = 0;
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in)) {
        if (in->inbox_full) rc = ESPMOD_ERR_BUSY;
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
    em_event_t ev = {.kind = EM_EV_REQUEST, .inst = id, .component = component, .method = method};
    if (xQueueSend(worker_queue, &ev, 0) != pdTRUE) {
        portENTER_CRITICAL(&registry_lock);
        if (known_locked(in)) { in->inbox_full = false; if (sem) in->waiter_sem = NULL; }
        portEXIT_CRITICAL(&registry_lock);
        return ESPMOD_ERR_BUSY;
    }
    return 0;
}

int espmod_instance_post(espmod_instance_t *in, int component, int method, const uint8_t *data, size_t len)
{
    if (!in || (len && !data)) return ESPMOD_ERR_ARG;
    if (len > CONFIG_ESPMOD_REQUEST_MAX) return ESPMOD_ERR_SIZE;
    return post_locked_copy(in, component, method, data, len, NULL, NULL, 0, NULL, NULL);
}

int espmod_instance_request(espmod_instance_t *in, int component, int method, const uint8_t *data, size_t len,
                            uint8_t *response, size_t capacity, size_t *response_len, uint32_t timeout_ms)
{
    if (!in || (len && !data) || (capacity && !response) || !response_len) return ESPMOD_ERR_ARG;
    if (len > CONFIG_ESPMOD_REQUEST_MAX) return ESPMOD_ERR_SIZE;
    *response_len = 0;
    SemaphoreHandle_t sem = xSemaphoreCreateBinary();
    if (!sem) return ESPMOD_ERR_NOMEM;
    int result = ESPMOD_ERR_TIMEOUT;
    int rc = post_locked_copy(in, component, method, data, len, sem, response, capacity, response_len, &result);
    if (!rc && xSemaphoreTake(sem, pdMS_TO_TICKS(timeout_ms ? timeout_ms : 1000)) != pdTRUE) {
        portENTER_CRITICAL(&registry_lock);
        const bool pending = known_locked(in) && in->waiter_sem == sem;
        if (pending) in->waiter_sem = NULL;   /* the worker will not touch the caller's buffers any more */
        portEXIT_CRITICAL(&registry_lock);
        if (!pending) (void)xSemaphoreTake(sem, portMAX_DELAY);   /* it was just answered: the give is on its way */
        else result = ESPMOD_ERR_TIMEOUT;
    } else if (rc) {
        result = rc;
    }
    vSemaphoreDelete(sem);
    return result;
}

void espmod_instance_stats(const espmod_instance_t *in, espmod_stats_t *out)
{
    if (!out) return;
    memset(out, 0, sizeof(*out));
    portENTER_CRITICAL(&registry_lock);
    if (known_locked(in)) *out = in->stats;
    portEXIT_CRITICAL(&registry_lock);
}
