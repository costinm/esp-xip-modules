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
static void serial_release(struct espmod_instance *in);

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

static void *h_alloc(void *user, size_t size, size_t align)
{
    struct espmod_instance *in = I(user);
    if (size == 0 || align == 0 || (align & (align - 1)) != 0) return NULL;
    if (!in->arena) {
        in->arena = heap_caps_malloc(CONFIG_ESPMOD_ARENA_SIZE + 15, MALLOC_CAP_8BIT);
        if (!in->arena) return NULL;
        in->arena_used = 0;
    }
    uintptr_t base = ((uintptr_t)in->arena + 15) & ~(uintptr_t)15;
    uintptr_t at = (base + in->arena_used + align - 1) & ~(uintptr_t)(align - 1);
    if (at < base || at - base > CONFIG_ESPMOD_ARENA_SIZE || size > CONFIG_ESPMOD_ARENA_SIZE - (at - base)) return NULL;
    in->arena_used = (at - base) + size;
    return (void *)at;
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

/* A line arrived (serial task): the owner's answer, as text, goes back to the port. */
static void on_serial_line(void *ctx, char *line, size_t len)
{
    (void)ctx;
    static uint8_t answer[CONFIG_ESPMOD_RESPONSE_MAX];
    portENTER_CRITICAL(&registry_lock);
    struct espmod_instance *owner = serial_owner;
    const int component = serial_component, method = serial_method;
    portEXIT_CRITICAL(&registry_lock);
    size_t n = 0;
    if (owner && espmod_instance_request(owner, component, method, (const uint8_t *)line, len, answer, sizeof(answer), &n, 5000) == 0 && n)
        espmod_serial_write(answer, n);
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

static void settle_after_call(struct espmod_instance *t, int rc);

/* The target's handler runs here, nested in the caller's: still one handler at a time on the worker. */
static int h_call(void *user, int component, int method, const uint8_t *data, size_t len, uint8_t *resp, size_t cap, size_t *rlen)
{
    struct espmod_instance *me = I(user);
    if (rlen) *rlen = 0;
    if (xTaskGetCurrentTaskHandle() != worker_task || (len && !data) || (cap && !resp)) return ESPMOD_ERR_ARG;
    struct espmod_instance *t = component ? by_component(component, me) : NULL;
    if (!t) return ESPMOD_ERR_NOTFOUND;
    if (t->in_request) return ESPMOD_ERR_BUSY;
    if (!t->response && !(t->response = malloc(CONFIG_ESPMOD_RESPONSE_MAX))) return ESPMOD_ERR_NOMEM;
    t->in_request = true; t->responded = false; t->response_len = 0;
    const int rc = dispatch(t, component, method, 0, 1, data, len);
    t->in_request = false;
    size_t n = t->responded ? t->response_len : 0;
    int ret = ESPMOD_OK;
    if (n > cap) { n = cap; ret = ESPMOD_ERR_SIZE; }
    if (n) memcpy(resp, t->response, n);
    if (rlen) *rlen = n;
    settle_after_call(t, rc);
    return ret;
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
    em_periph_release(in);
    espmod_done_fn done = in->done;
    void *ctx = in->done_ctx;
    espmod_stats_t stats = in->stats;
    ESP_LOGI(TAG, "%s ended: %d (%u calls, max %u us)", in->name, result, (unsigned)stats.calls, (unsigned)stats.max_handler_us);
    free(in->state); free(in->inbox); free(in->start_request); free(in->response); free(in->arena);
    free(in);
    if (done) done(ctx, result, &stats);
}

static int dispatch(struct espmod_instance *in, int component, int method, int value, uint32_t flags,
                    const uint8_t *data, size_t len)
{
    const espmod_call call = {.size = sizeof(call), .component = component, .method = method, .value = value,
                              .flags = flags, .data = data, .len = len};
    in->arena_used = 0;
    const int64_t t0 = esp_timer_get_time();
    int rc = in->handler(&in->table, in->state, &call);
    uint32_t us = (uint32_t)(esp_timer_get_time() - t0);
    in->stats.calls++;
    if (us > in->stats.max_handler_us) in->stats.max_handler_us = us;
    if (us / 1000 > CONFIG_ESPMOD_HANDLER_MS) {
        in->stats.overruns++;
        ESP_LOGW(TAG, "%s: handler %d.%d took %u ms (budget %d)", in->name, component, method, (unsigned)(us / 1000),
                 CONFIG_ESPMOD_HANDLER_MS);
    }
    return rc;
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

static void settle_after_call(struct espmod_instance *t, int rc) { settle(t, rc); }

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
        h.loader_start = ext->loader_start; h.loader_stop = ext->loader_stop;
        h.modules_info = ext->modules_info; h.module_start = ext->module_start; h.module_stop = ext->module_stop;
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

espmod_instance_t *espmod_instance_create(const char *name, const espmod_instance_config_t *cfg)
{
    if (!inited || !name || !*name || !cfg || !cfg->handler || cfg->state_bytes > 4096) return NULL;
    struct espmod_instance *in = calloc(1, sizeof(*in));
    if (!in) return NULL;
    strlcpy(in->name, name, sizeof(in->name));
    in->handler = cfg->handler; in->done = cfg->done; in->done_ctx = cfg->done_ctx;
    in->state_bytes = cfg->state_bytes;
    in->component = cfg->component;
    in->state = calloc(1, cfg->state_bytes ? cfg->state_bytes : 1);
    in->inbox = malloc(CONFIG_ESPMOD_REQUEST_MAX);
    if (!in->state || !in->inbox) { free(in->state); free(in->inbox); free(in); return NULL; }
    in->table = (espmod_host){
        .abi_version = ESPMOD_ABI_VERSION,
        .size = sizeof(espmod_host),
        .features = ESPMOD_FEAT_SETTINGS | ESPMOD_FEAT_SPI | ESPMOD_FEAT_SPI_READY | ESPMOD_FEAT_I2C |
                    ESPMOD_FEAT_ADC | ESPMOD_FEAT_RGBLED | ESPMOD_FEAT_SERVICE |
                    (hooks.next_window_ms ? ESPMOD_FEAT_ALARM_FLEX : 0) | (hooks.wifi_start ? ESPMOD_FEAT_WIFI : 0) | (hooks.loader_start ? ESPMOD_FEAT_LOADER : 0) |
                    ESPMOD_FEAT_SERIAL | ESPMOD_FEAT_CALL | (hooks.modules_info ? ESPMOD_FEAT_MODULES : 0),
        .user = in,
        .log = h_log, .alloc = h_alloc, .now_ms = h_now_ms, .respond = h_respond, .emit = h_emit,
        .call_service = h_call_service, .get_setting = h_get_setting, .set_setting = h_set_setting,
        .alarm_set = h_alarm_set, .alarm_cancel = h_alarm_cancel, .connected = h_connected,
        .wifi_start = h_wifi_start, .wifi_stop = h_wifi_stop, .wifi_info = h_wifi_info,
        .loader_start = h_loader_start, .loader_stop = h_loader_stop,
        .serial_open = h_serial_open, .serial_write = h_serial_write, .serial_close = h_serial_close, .call = h_call,
        .modules_info = h_modules_info, .module_start = h_module_start, .module_stop = h_module_stop,
        .events_info = h_events_info, .reset = h_reset,
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
