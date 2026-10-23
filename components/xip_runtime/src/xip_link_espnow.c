/* ESP-NOW raw action bearer. Wi-Fi setup and callback registration belong to
 * xip_wifi.c; this file owns parsing, pooled packets and callback lifetimes.
 * The native RX buffers belong to ESP-IDF: decode once into a runtime lease,
 * then commit ownership to the link. No private packet queue or frame scratch. */
#include <stdatomic.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "xip_wifi.h"
/* Raw NOW framing is local to this bearer; exported wrappers below serve ESP adapters. */
#define XIP_NOW_IEEE80211_HEADER 24
#define XIP_NOW_ACTION_HEADER 8
#define XIP_NOW_IE_HEADER 7
#define XIP_NOW_RAW_PREFIX 16
#define XIP_NOW_MAX_PAYLOAD XIP_PKT_MAX
#define XIP_NOW_VENDOR_IE_ID 0xdd
#define XIP_NOW_ACTION_TYPE 0x04
#define XIP_NOW_V2 2
static const uint8_t xip_now_prefix[4] = {0x7f, 0x18, 0xfe, 0x34};
static inline size_t xip_now_frame_len(size_t len)
{
    return XIP_NOW_IEEE80211_HEADER + XIP_NOW_RAW_PREFIX + len;
}
/* One pad byte in the vendor IE makes the action prefix 16 bytes. At the
 * standard 144-byte payload offset the native request and body are aligned. */
static inline size_t xip_now_action_prefix(uint8_t *out, size_t cap, size_t len)
{
    if (len > XIP_NOW_MAX_PAYLOAD || cap < XIP_NOW_RAW_PREFIX) return 0;
    memcpy(out, xip_now_prefix, 4);
    memset(out + 4, 0xff, 4);
    out[8] = XIP_NOW_VENDOR_IE_ID; out[9] = 6;
    memcpy(out + 10, xip_now_prefix + 1, 3);
    out[13] = XIP_NOW_ACTION_TYPE; out[14] = XIP_NOW_V2; out[15] = 0;
    return XIP_NOW_RAW_PREFIX;
}
static inline size_t xip_now_encode(uint8_t *out, size_t cap, const uint8_t dest[6], const uint8_t src[6], const uint8_t bssid[6],
                                    const uint8_t *payload, size_t len)
{
    const size_t used = xip_now_frame_len(len);
    if (len > XIP_NOW_MAX_PAYLOAD || cap < used) return 0;
    memset(out, 0, XIP_NOW_IEEE80211_HEADER); out[0] = 0xd0;
    memcpy(out + 4, dest, 6); memcpy(out + 10, src, 6); memcpy(out + 16, bssid, 6);
    (void)xip_now_action_prefix(out + XIP_NOW_IEEE80211_HEADER, cap - XIP_NOW_IEEE80211_HEADER, len);
    if (len) memcpy(out + XIP_NOW_IEEE80211_HEADER + XIP_NOW_RAW_PREFIX, payload, len);
    return used;
}
/* Validate the vendor envelope and skip the complete first IE, including any
 * padding. All remaining bytes are the payload, including a four-byte tail.
 * A raw-capture caller must remove FCS using capture metadata before calling. */
static inline long xip_now_parse_body(const uint8_t *action, size_t len, uint8_t *out, size_t cap)
{
    if (len < XIP_NOW_ACTION_HEADER + XIP_NOW_IE_HEADER || memcmp(action, xip_now_prefix, 4)) return -1;
    const uint8_t *ie = action + XIP_NOW_ACTION_HEADER;
    if (ie[0] != XIP_NOW_VENDOR_IE_ID || ie[1] < 5 || memcmp(ie + 2, xip_now_prefix + 1, 3) ||
        ie[5] != XIP_NOW_ACTION_TYPE || (ie[6] & 0x0f) != XIP_NOW_V2) return -1;
    const size_t start = XIP_NOW_ACTION_HEADER + 2 + ie[1];
    if (start > len || len - start > cap || len - start > XIP_NOW_MAX_PAYLOAD) return -1;
    if (len > start) memcpy(out, action + start, len - start);
    return (long)(len - start);
}
static inline long xip_now_parse_frame(const uint8_t *frame, size_t len, uint8_t src[6], uint8_t *out, size_t cap)
{
    if (len < XIP_NOW_IEEE80211_HEADER || frame[0] != 0xd0) return -1;
    long used = xip_now_parse_body(frame + XIP_NOW_IEEE80211_HEADER, len - XIP_NOW_IEEE80211_HEADER, out, cap);
    if (used >= 0) memcpy(src, frame + 10, 6);
    return used;
}


#define NOW_ACTION_TX_WAIT_MS 1000
#define NOW_TX_RETRY_MS 10
#define ACTION_BODY_MAX 2346

/* Only the gate/pointer and configuration survive an epoch. The hook cannot be
 * unregistered; callbacks acquire a reference under the gate before using state. */
typedef struct {
    int link;
    unsigned users;
    atomic_bool tx_busy;
    esp_timer_handle_t retry_timer;
#if CONFIG_XIP_ESPNOW_DIAGNOSTICS
    xip_espnow_stats_t stats;
#endif
} now_state;
static portMUX_TYPE state_lock = portMUX_INITIALIZER_UNLOCKED;
static now_state *active;
static atomic_bool mac_ack_enabled = true;

static now_state *acquire(void)
{
    portENTER_CRITICAL(&state_lock);
    now_state *s = active;
    if (s) s->users++;
    portEXIT_CRITICAL(&state_lock);
    return s;
}
static void release(now_state *s)
{
    portENTER_CRITICAL(&state_lock);
    s->users--;
    portEXIT_CRITICAL(&state_lock);
}
#if CONFIG_XIP_ESPNOW_DIAGNOSTICS
#define COUNT(s, field) do { portENTER_CRITICAL(&state_lock); (s)->stats.field++; portEXIT_CRITICAL(&state_lock); } while (0)
#define LAST_ERROR(s, value) do { portENTER_CRITICAL(&state_lock); (s)->stats.tx_last_error = (value); portEXIT_CRITICAL(&state_lock); } while (0)
#else
#define COUNT(s, field) ((void)0)
#define LAST_ERROR(s, value) ((void)0)
#endif

void xip_link_espnow_set_mac_ack(bool on) { atomic_store(&mac_ack_enabled, on); }
static uint64_t mac_token(const uint8_t m[6])
{
    uint64_t t = 0;
    for (int i = 0; i < 6; i++) t |= (uint64_t)m[i] << (8 * i);
    return t;
}
static bool is_local_source(const uint8_t source[6])
{
    uint8_t mac[6];
    if (xip_wifi_interface_mac(WIFI_IF_STA, mac) && !memcmp(mac, source, 6)) return true;
    return xip_wifi_ap_only_active() && xip_wifi_interface_mac(WIFI_IF_AP, mac) && !memcmp(mac, source, 6);
}
static void deliver(now_state *s, const uint8_t *body, size_t len, const uint8_t source[6])
{
    if (is_local_source(source)) { COUNT(s, self_echoes); return; }
    xip_link_rx_buf buf;
    const int rc = xip_link_native_rx_begin_cached(s->link, &buf);
    if (rc == 1) return;
    if (rc) { COUNT(s, drops); return; }
    const size_t cap = buf.capacity < XIP_NOW_MAX_PAYLOAD ? buf.capacity : XIP_NOW_MAX_PAYLOAD;
    long used = xip_now_parse_body(body, len, buf.data, cap);
    if (used < 0) { xip_link_native_rx_abort(&buf); COUNT(s, parse); COUNT(s, drops); return; }
    COUNT(s, actions);
    /* Commit consumes this reference. Fan-out lends it to consumers; retained
     * packets stay owned by them until send/free. No driver-held RX copy. */
    xip_link_native_rx_commit(s->link, &buf, (size_t)used, mac_token(source), 0);
}

/* Registered by the radio owner. Generic STA form legitimately has first word 0. */
int xip_link_espnow_action_rx(void *context, size_t second, uint8_t *third, uint8_t *fourth)
{
    now_state *s = acquire();
    if (!s) return 0;
    COUNT(s, hook_calls);
    uint8_t source[6];
    const uint8_t *body = third;
    size_t len;
    if (!third) goto invalid;
    if (second <= ACTION_BODY_MAX) {
        const uint8_t *mac;
        if (!context) goto invalid;
        memcpy(&mac, context, sizeof(mac));
        if (!mac) goto invalid;
        memcpy(source, mac, 6); len = second;
    } else {
        const uint8_t *header = (const uint8_t *)second;
        if ((uintptr_t)fourth < (uintptr_t)third || (uintptr_t)fourth - (uintptr_t)third > ACTION_BODY_MAX) goto invalid;
        if (!xip_rx_accept(header, 24)) { release(s); return 0; }
        memcpy(source, header + 10, 6); len = (size_t)((uintptr_t)fourth - (uintptr_t)third);
    }
    COUNT(s, dispatcher);
    deliver(s, body, len, source);
    release(s); return 0;
invalid:
    COUNT(s, invalid); COUNT(s, drops); release(s); return 0;
}

static int action_tx_rx_callback(uint8_t *header, uint8_t *payload, size_t len, uint8_t channel)
{
    (void)channel;
    now_state *s = acquire();
    if (!s) return 0;
    COUNT(s, tx_response_hook);
    if (!header || !payload || len > ACTION_BODY_MAX) { COUNT(s, invalid); COUNT(s, drops); }
    else if (xip_rx_accept(header, 24)) deliver(s, payload, len, header + 10);
    /* Parsing is reentrant: there is no global busy latch for a rejected
     * frame to strand, and every callback releases its lifetime reference. */
    release(s); return 0;
}
static void retry_ready(void *arg)
{
    (void)arg;
    now_state *s = acquire();
    if (s) { xip_link_native_ready(s->link); release(s); }
}
static void schedule_ready(now_state *s)
{
    (void)esp_timer_stop(s->retry_timer);
    (void)esp_timer_start_once(s->retry_timer, NOW_TX_RETRY_MS * 1000);
}

static int transmit(now_state *s, const uint8_t destination[6], const xip_link_packet *pkt)
{
    if (!pkt || !pkt->pool || !pkt->data) return XIP_ERR_ARG;
    if (pkt->len > XIP_PKT_MAX) return XIP_ERR_SIZE;
    const size_t prefix = XIP_NOW_RAW_PREFIX;
    const size_t header_size = offsetof(wifi_action_tx_req_t, data);
    const size_t needed = header_size + prefix;
    /* A packet passed for TX is an exclusive mutable lease, never an Arc-like
     * shared read reference. Reject contract violations; do not hide them with
     * allocation/copying. The runtime returns BUSY before calling us if borrowed. */
    if (xip_pool_refs(pkt->pool, pkt->slot) != 1 || pkt->headroom < needed ||
        (uintptr_t)(pkt->data - needed) % _Alignof(wifi_action_tx_req_t)) return XIP_ERR_ARG;
    wifi_action_tx_req_t *r = (wifi_action_tx_req_t *)(pkt->data - needed);
    memset(r, 0, header_size);
    if (!xip_now_action_prefix(r->data, prefix, pkt->len)) return XIP_ERR_SIZE;
    r->ifx = xip_wifi_ap_only_active() ? WIFI_IF_AP : WIFI_IF_STA;
    memcpy(r->dest_mac, destination, 6);
    r->type = WIFI_OFFCHAN_TX_REQ;
    uint8_t channel; wifi_second_chan_t secondary;
    int result = XIP_ERR_IO;
    if (!xip_wifi_current_channel(&channel, &secondary)) return XIP_ERR_IO;
    r->channel = channel; r->sec_channel = secondary;
    r->wait_time_ms = NOW_ACTION_TX_WAIT_MS;
    static const uint8_t broadcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    r->no_ack = !memcmp(destination, broadcast, 6) || !atomic_load(&mac_ack_enabled);
    r->rx_cb = action_tx_rx_callback;
    memcpy(r->bssid, broadcast, 6);
    r->data_len = (uint32_t)(prefix + pkt->len);
    /* ESP-IDF has no tx_by_ref for raw action frames. Its synchronous API
     * allocates/copies internally; there is nothing we can avoid on our side
     * for now. The original pool lease remains the only adapter buffer. */
    const esp_err_t sent = esp_wifi_action_tx_req(r);
    LAST_ERROR(s, sent);
    if (sent == ESP_OK) { COUNT(s, tx); result = XIP_LINK_TX_DONE; }
    else { COUNT(s, tx_failures); result = sent == ESP_ERR_NO_MEM ? XIP_ERR_BUSY : XIP_ERR_IO; }
    /* IDF's synchronous ioctl copies request+body before returning (verified
     * in the pinned 6.0.2 library). This is submission, not receiver success.
     * Runtime releases the original lease on DONE; BUSY retains it unchanged. */
    return result;
}
static int link_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    (void)ctx;
    now_state *s = acquire();
    if (!s) return XIP_ERR_UNSUPPORTED;
    uint8_t destination[6];
    if (!dest) memset(destination, 0xff, 6);
    else for (int i = 0; i < 6; i++) destination[i] = (uint8_t)(dest >> (8 * i));
    int rc;
    if (atomic_exchange(&s->tx_busy, true)) rc = XIP_ERR_BUSY;
    else { rc = transmit(s, destination, pkt); atomic_store(&s->tx_busy, false); }
    if (rc == XIP_ERR_BUSY) schedule_ready(s);
    release(s); return rc;
}
int xip_link_espnow_start(void)
{
    /* Start/stop are serialized by the radio owner, never called by RX. */
    portENTER_CRITICAL(&state_lock);
    const bool running = active != NULL;
    portEXIT_CRITICAL(&state_lock);
    if (running) return XIP_OK;
    now_state *s = calloc(1, sizeof(*s));
    if (!s) return XIP_ERR_NOMEM;
    atomic_init(&s->tx_busy, false);
    const esp_timer_create_args_t args = {.callback = retry_ready, .dispatch_method = ESP_TIMER_TASK, .name = "espnow-ready", .skip_unhandled_events = true};
    if (esp_timer_create(&args, &s->retry_timer) != ESP_OK) { free(s); return XIP_ERR_NOMEM; }
    s->link = xip_link_native_register("espnow", XIP_PKT_MAX, 0, link_tx, NULL);
    if (s->link < 0) { esp_timer_delete(s->retry_timer); int rc = s->link; free(s); return rc; }
    xip_link_native_rx_prepare(); /* Allocate on the owner, never in RX. */
    portENTER_CRITICAL(&state_lock); active = s; portEXIT_CRITICAL(&state_lock);
    (void)xip_link_native_set_state(s->link, XIP_LINK_UP, 1000);
    return XIP_OK;
}
void xip_link_espnow_stop(void)
{
    portENTER_CRITICAL(&state_lock);
    now_state *s = active; active = NULL;
    portEXIT_CRITICAL(&state_lock);
    if (!s) return;
    (void)xip_link_native_set_state(s->link, XIP_LINK_DOWN, 0);
    /* Detach first; late vendor/response/timer callbacks see no state. Wait
     * for entered callbacks/TX calls before deleting any epoch resources. */
    for (;;) {
        portENTER_CRITICAL(&state_lock); unsigned users = s->users; portEXIT_CRITICAL(&state_lock);
        if (!users) break;
        vTaskDelay(1);
    }
    (void)esp_timer_stop(s->retry_timer);
    (void)esp_timer_delete(s->retry_timer);
    free(s);
}
void xip_link_espnow_stats(xip_espnow_stats_t *out)
{
    if (!out) return;
    memset(out, 0, sizeof(*out));
#if CONFIG_XIP_ESPNOW_DIAGNOSTICS
    portENTER_CRITICAL(&state_lock);
    if (active) *out = active->stats;
    portEXIT_CRITICAL(&state_lock);
#endif
}

/* Wire helpers shared with the product Rust adapter: pinned portable dependencies
 * need not carry a second, stale ESP codec while the host peer update lands. */
size_t xip_espnow_frame_encode(uint8_t *out, size_t cap, const uint8_t dst[6], const uint8_t src[6], const uint8_t bssid[6], const uint8_t *data, size_t len)
{
    return xip_now_encode(out, cap, dst, src, bssid, data, len);
}
long xip_espnow_body_decode(const uint8_t *body, size_t len, uint8_t *out, size_t cap)
{
    return xip_now_parse_body(body, len, out, cap);
}
long xip_espnow_frame_decode(const uint8_t *frame, size_t len, uint8_t source[6], uint8_t *out, size_t cap)
{
    return xip_now_parse_frame(frame, len, source, out, cap);
}
