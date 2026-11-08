/* SDK boundary fake for Rust tests of the actual C driver. No copied driver logic. */
#include <assert.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include "xip_wifi.h"
#include "esp_timer.h"
static atomic_uint contexts, timers;
static unsigned ready_count;
static atomic_bool hold_rx, rx_entered;
static void *state_alloc(size_t n, size_t size) { void *p = calloc(n, size); if (p) contexts++; return p; }
static void state_free(void *p) { if (p) contexts--; free(p); }
#define calloc state_alloc
#define free state_free
#include "../../../../../components/xip_runtime/src/xip_link_espnow.c"
#undef calloc
#undef free

static xip_pool *test_pool;
static xip_link_tx_fn send_packet;
static int received_slot = -1, sent_error;
static unsigned received, sent, reused, aborted;
static bool accept_frame = true;
static uint8_t *original;
static size_t original_size;
static uint8_t captured[XIP_PKT_MAX + XIP_NOW_RAW_PREFIX];
static size_t captured_len;
static wifi_action_rx_cb_t reply_callback;
struct test_timer { esp_timer_create_args_t args; };
int64_t esp_timer_get_time(void) { return 100000; }
esp_err_t esp_timer_create(const esp_timer_create_args_t *a, esp_timer_handle_t *out) {
    *out = malloc(sizeof(**out)); assert(*out); (*out)->args = *a; timers++; return 0;
}
esp_err_t esp_timer_start_once(esp_timer_handle_t t, uint64_t us) { (void)t; (void)us; ready_count++; return 0; }
esp_err_t esp_timer_stop(esp_timer_handle_t t) { (void)t; return 0; }
esp_err_t esp_timer_delete(esp_timer_handle_t t) { timers--; free(t); return 0; }
void vTaskDelay(unsigned ticks) { (void)ticks; sched_yield(); }
int xip_link_native_register(const char *name, uint32_t mtu, uint32_t flags, xip_link_tx_fn tx, void *ctx) {
    assert(!strcmp(name, "espnow") && mtu == XIP_PKT_MAX); (void)flags; (void)ctx; send_packet = tx; return 256;
}
int xip_link_native_set_state(int link, uint32_t state, uint32_t rate) { (void)link; (void)state; (void)rate; return 0; }
void xip_link_native_unregister(int link){(void)link;}
void xip_link_native_ready(int link) { (void)link; }
void xip_link_native_rx_prepare(void) { xip_pool_prepare_rx(test_pool); }
int xip_link_native_rx_begin_cached(int link, xip_link_rx_buf *buf) {
    (void)link;
    if (atomic_load(&hold_rx)) {
        atomic_store(&rx_entered, true);
        while (atomic_load(&hold_rx)) sched_yield();
    }
    int slot = xip_pool_acquire_cached(test_pool, 0); if (slot < 0) return XIP_ERR_NOMEM;
    buf->data = xip_pool_slot(test_pool, slot) + XIP_PKT_HEADROOM;
    buf->capacity = XIP_PKT_MAX; buf->slot = slot; return 0;
}
void xip_link_native_rx_commit(int link, const xip_link_rx_buf *buf, size_t len, uint64_t src, int32_t signal) {
    (void)link; (void)signal; assert(src == 0x070605040302ULL && len == 4);
    assert(!memcmp(buf->data, "ping", 4)); assert(received_slot < 0);
    received++; received_slot = buf->slot;
}
void xip_link_native_rx_abort(const xip_link_rx_buf *buf) { aborted++; xip_pool_release(test_pool, buf->slot); }
bool xip_wifi_interface_mac(wifi_interface_t interface, uint8_t mac[6]) { (void)interface; memset(mac, 0, 6); return true; }
bool xip_wifi_ap_only_active(void) { return false; }
bool xip_wifi_current_channel(uint8_t *channel, wifi_second_chan_t *secondary) { *channel = 6; *secondary = WIFI_SECOND_CHAN_NONE; return true; }
bool xip_rx_accept(const uint8_t *frame, size_t len) { (void)frame; (void)len; return accept_frame; }
esp_err_t esp_wifi_action_tx_req(wifi_action_tx_req_t *request) {
    assert((uintptr_t)request % _Alignof(wifi_action_tx_req_t) == 0);
    assert(request->wait_time_ms == 1000 && request->channel == 6 && !request->no_ack);
    reply_callback = request->rx_cb; sent++;
    reused = (uintptr_t)request >= (uintptr_t)original && (uintptr_t)request < (uintptr_t)original + original_size;
    assert(request->data_len <= sizeof(captured)); captured_len = request->data_len;
    memcpy(captured, request->data, captured_len); return sent_error;
}
esp_err_t xip_wifi_action_tx_req(wifi_action_tx_req_t *request) {return esp_wifi_action_tx_req(request);}
void test_begin(unsigned slots) {
    assert(contexts == 0 && timers == 0 && received_slot < 0);
    if (test_pool) xip_pool_destroy(test_pool);
    test_pool = xip_pool_create(slots, XIP_PKT_SLOT_MIN); assert(test_pool);
    received = sent = ready_count = reused = aborted = 0; sent_error = 0; accept_frame = true;
    assert(xip_link_espnow_start() == 0);
}
unsigned test_contexts(void) { return contexts; }
unsigned test_timers(void) { return timers; }
unsigned test_received(void) { return received; }
unsigned test_aborted(void) { return aborted; }
unsigned test_available(void) { return xip_pool_available(test_pool); }
void test_consume(void) { assert(received_slot >= 0); xip_pool_release(test_pool, received_slot); received_slot = -1; }
void test_receive(unsigned form, unsigned filtered) {
    uint8_t source[6] = {2,3,4,5,6,7}, local[6] = {0}, frame[64];
    size_t n = xip_now_encode(frame, sizeof(frame), local, source, local, (const uint8_t *)"ping", 4);
    accept_frame = !filtered;
    if (form == 0) xip_link_espnow_action_rx(NULL, (size_t)frame, frame + 24, frame + n);
    else if (form == 1) { const uint8_t *mac = source; xip_link_espnow_action_rx(&mac, n - 24, frame + 24, NULL); }
    else { assert(reply_callback); reply_callback(frame, frame + 24, n - 24, 6); }
}
unsigned test_aligned_offset(void) {
    size_t n = offsetof(wifi_action_tx_req_t, data) + XIP_NOW_RAW_PREFIX, align = _Alignof(wifi_action_tx_req_t);
    return (unsigned)(n + (XIP_PKT_HEADROOM - n + align - 1) / align * align);
}
int test_send(unsigned offset, unsigned shared, int error) {
    int slot = xip_pool_acquire(test_pool, 0); assert(slot >= 0);
    original = xip_pool_slot(test_pool, slot); original_size = xip_pool_slot_size(test_pool);
    memset(original, 0xa5, original_size); memcpy(original + offset, "ping", 4);
    if (shared) xip_pool_retain(test_pool, slot);
    xip_link_packet packet = {.data = original + offset, .len = 4, .headroom = offset, .slot = slot, .pool = test_pool};
    sent_error = error;
    int result = send_packet(NULL, 0x070605040302ULL, &packet);
    assert(!memcmp(original + offset, "ping", 4));
    assert(xip_pool_refs(test_pool, slot) == (shared ? 2u : 1u));
    if (shared) xip_pool_release(test_pool,slot);
    if (sent) { uint8_t out[4]; assert(xip_now_parse_body(captured, captured_len, out, 4) == 4); assert(!memcmp(out, "ping", 4)); }
    xip_pool_release(test_pool, slot); return result;
}
unsigned test_reused(void) { return reused; }
unsigned test_ready(void) { return ready_count; }

void test_hold_receive(unsigned hold) { atomic_store(&rx_entered, false); atomic_store(&hold_rx, hold); }
unsigned test_receive_entered(void) { return atomic_load(&rx_entered); }
unsigned test_enabled(void) {
    portENTER_CRITICAL(&state_lock); unsigned yes = active != NULL; portEXIT_CRITICAL(&state_lock); return yes;
}
size_t test_encode(uint8_t *out, size_t cap, const uint8_t *dest, const uint8_t *source, const uint8_t *bssid, const uint8_t *payload, size_t n) {
    return xip_now_encode(out, cap, dest, source, bssid, payload, n);
}
long test_parse_body(const uint8_t *body, size_t len, uint8_t *out, size_t cap) { return xip_now_parse_body(body, len, out, cap); }

void test_finish(void) {
    assert(contexts == 0 && timers == 0 && received_slot < 0);
    assert(xip_pool_available(test_pool) == 3);
    xip_pool_destroy(test_pool); test_pool = NULL;
}

void test_cache_exhaustion(void) {
    int a = xip_pool_acquire_cached(test_pool, 0), b = xip_pool_acquire_cached(test_pool, 0);
    assert(a >= 0 && b >= 0 && xip_pool_available(test_pool) == 1);
    unsigned before = received;
    test_receive(0, 0);
    assert(received == before && xip_pool_allocated(test_pool) == 2);
    xip_pool_release(test_pool, a); xip_pool_release(test_pool, b);
}

/* SDK-boundary fake checks retained pointer lifetime and mode-scoped votes.
 * It models API calls only; RF reception requires device evidence. */
static const uint16_t *sdk_window;
static unsigned setup_fault, window_registrations, hook_registrations;
esp_err_t esp_wifi_internal_configure_wake_window(bool enable, const uint16_t *window) {
    assert(window && *window == UINT16_MAX);
    if ((enable && setup_fault == 1) || (!enable && setup_fault == 3)) return ESP_ERR_NO_MEM;
    if (enable) { assert(!sdk_window); sdk_window = window; window_registrations++; }
    else { assert(sdk_window == window); sdk_window = NULL; }
    return ESP_OK;
}
int ieee80211_recv_action_register(uint8_t category, uint8_t action, int (*cb)(void *, size_t, uint8_t *, uint8_t *)) {
    assert(category == 127 && action == 0 && cb);
    hook_registrations++; return setup_fault == 2 ? -1 : 0;
}
#include "wifi_now_setup.inc"
void test_radio_setup(void) {
    setup_fault = 1;
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, true) == XIP_ERR_IO && !sdk_window);
    setup_fault = 2;
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, true) == XIP_ERR_IO && !sdk_window);
    setup_fault = 0; window_registrations = hook_registrations = 0;
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, true) == XIP_OK);
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, true) == XIP_OK);
    assert(window_registrations == 1 && hook_registrations == 2 && sdk_window);
    setup_fault = 3;
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, false) == XIP_ERR_IO && sdk_window);
    assert(xip_wifi_now_disable() == XIP_ERR_IO && sdk_window && *sdk_window == UINT16_MAX);
    setup_fault = 0;
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, false) == XIP_OK && !sdk_window);
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, false) == XIP_OK && !sdk_window);
    assert(window_registrations == 1);
    assert(xip_wifi_now_enable(xip_link_espnow_action_rx, true) == XIP_OK && sdk_window);
    assert(xip_wifi_now_disable() == XIP_OK && !sdk_window);
    assert(xip_wifi_now_disable() == XIP_OK);
    assert(xip_wifi_now_enable(NULL, true) == XIP_ERR_ARG);
}

/* The same exported owner entry is installed by both C and Rust hosts. */
static bool started;
static unsigned monitor_calls;
int xip_link_promisc_set(const xip_wifi_monitor *monitor) {
    assert(monitor && monitor->size==sizeof(*monitor)); monitor_calls++; return 27;
}
#include "wifi_monitor.inc"
void test_monitor_owner(void) {
    xip_wifi_monitor monitor={.size=sizeof(monitor)};
    assert(xip_wifi_owner_monitor(NULL)==XIP_ERR_ARG);
    monitor.size--;
    assert(xip_wifi_owner_monitor(&monitor)==XIP_ERR_ARG);
    monitor.size++;
    assert(xip_wifi_owner_monitor(&monitor)==XIP_ERR_UNSUPPORTED && monitor_calls==0);
    started=true;
    assert(xip_wifi_owner_monitor(&monitor)==27 && monitor_calls==1);
    started=false;
}
