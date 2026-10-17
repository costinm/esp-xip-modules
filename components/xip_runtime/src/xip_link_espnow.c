/* ESP-NOW as the "espnow" link: DMesh's action-frame bearer (wifi_espnow_esp.rs), ported with its driver behaviour intact. The Rust twin is
 * crates/xip-runtime-esp/src/espnow_esp.rs and the two are meant to behave the same.
 *
 * Receive is the private non-promiscuous vendor-action dispatcher (ieee80211_recv_action_register(127, 0, ...)), which has two call
 * forms, plus the in-band reply hook of an action transmit; transmit is esp_wifi_action_tx_req. The frames are DMesh's
 * (xip_espnow_frame.h). What changed is only where the datagram goes: DMesh parses into its QUIC node's pool and classifies the payload;
 * here it is parsed straight into a runtime pool packet and delivered to the subscribers of the link, or forwarded by a route.
 * Why raw action frames with the Espressif OUI, and nothing else: see ESPNOW.md. Started with the radio (xip_wifi.c wifi_up), stopped before it. */
#include <stdatomic.h>
#include <string.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "xip_wifi.h"
#include "xip_espnow_frame.h"

/* private libnet80211 receive registration: lifecycle state, not ESP-NOW framing */
int ieee80211_recv_action_register(uint8_t category, uint8_t action, int (*callback)(void *, size_t, uint8_t *, uint8_t *));

#define NOW_ACTION_TX_WAIT_MS 1000           /* unicast frames use the Wi-Fi hardware retry path; broadcast is never acknowledged in 802.11 */
#define NOW_TX_RETRY_MS 10                   /* a busy driver (ESP_ERR_NO_MEM) asks the sender to try again in this long */
#define LINK_MTU 1000                        /* DMesh carries 1400, the pool packet 1000 */
#define DUPLICATE_ACTION_WINDOW_MS 20        /* only a near-simultaneous duplicate from the two receive callbacks */
#define FRAME_CAPACITY (XIP_NOW_MAX_PAYLOAD + 96)

static int link_addr = -1;
static atomic_bool enabled;                  /* the private hook cannot be unregistered: stopping makes the callback inert */
static atomic_bool mac_ack_enabled = true;
static atomic_bool action_parse_busy;        /* the callbacks can preempt one another while they use the parser */
static atomic_bool tx_busy;                  /* one action request buffer: transmits are serialized */
static esp_timer_handle_t retry_timer;
static atomic_uint last_fingerprint, last_action_ms;

static atomic_uint rx_actions, rx_drops, rx_dispatcher, rx_tx_response_hook, rx_invalid_drops, rx_busy_drops, rx_parse_drops;
static atomic_uint rx_self_echoes, rx_duplicates, tx_actions, tx_failures;
static atomic_int tx_last_error;

/* the request storage of esp_wifi_action_tx_req: the SDK copies it before returning, and this scratch avoids a per-packet allocation */
typedef struct {
    wifi_action_tx_req_t request;
    uint8_t data[FRAME_CAPACITY - 24];
} action_tx_request_t;
typedef struct {
    uint8_t tx_frame[FRAME_CAPACITY];
    action_tx_request_t action_tx_request;
} action_buffers_t;
static action_buffers_t *buffers;            /* allocated once before the callbacks are registered; callbacks never allocate */

void xip_link_espnow_set_mac_ack(bool on) { atomic_store(&mac_ack_enabled, on); }

static uint32_t now_ms(void) { return (uint32_t)(esp_timer_get_time() / 1000); }
static uint64_t mac_token(const uint8_t m[6])
{
    uint64_t t = 0;
    for (int i = 0; i < 6; i++) t |= (uint64_t)m[i] << (8 * i);
    return t;
}

static bool is_local_action_source(const uint8_t source[6])
{
    uint8_t mac[6];
    if (xip_wifi_interface_mac(WIFI_IF_STA, mac) && !memcmp(mac, source, 6)) return true;
    return xip_wifi_ap_only_active() && xip_wifi_interface_mac(WIFI_IF_AP, mac) && !memcmp(mac, source, 6);
}

/* FNV-1a of the source and payload within a short window; no allocation or retained payload. */
static bool recently_seen_action(const uint8_t source[6], const uint8_t *payload, size_t len)
{
    uint32_t hash = 0x811c9dc5u;
    for (int i = 0; i < 6; i++) { hash ^= source[i]; hash *= 0x01000193u; }
    for (size_t i = 0; i < len; i++) { hash ^= payload[i]; hash *= 0x01000193u; }
    if (!hash) hash = 1;                    /* zero is the uninitialized value */
    const uint32_t now = now_ms();
    const bool duplicate = atomic_load(&last_fingerprint) == hash && (uint32_t)(now - atomic_load(&last_action_ms)) <= DUPLICATE_ACTION_WINDOW_MS;
    atomic_store(&last_fingerprint, hash);
    atomic_store(&last_action_ms, now);
    return duplicate;
}

/* Parse straight into a pool packet and hand it to the link (DMesh's receive_now_with). A locally transmitted action is not ingress,
 * and a near-simultaneous duplicate from the two callbacks must not spend two packets. `body`: the action body; `source`: the
 * sender; `reply_fallback`: the response hook's variant (the driver may have removed the vendor elements and kept the 8-byte header). */
static void deliver(const uint8_t *body, size_t len, const uint8_t source[6], bool reply_fallback)
{
    if (link_addr < 0) return;
    xip_link_rx_buf buf;
    int rc = xip_link_native_rx_begin(link_addr, &buf);
    if (rc == 1) return;                    /* nobody listens */
    if (rc) { atomic_fetch_add(&rx_drops, 1); return; }
    size_t cap = buf.capacity < XIP_NOW_MAX_PAYLOAD ? buf.capacity : XIP_NOW_MAX_PAYLOAD;
    long used = xip_now_parse_body(body, len, buf.data, cap);
    if (used < 0 && reply_fallback && len > 8 && !memcmp(body, xip_now_prefix, 4) && len - 8 <= cap) {
        memcpy(buf.data, body + 8, len - 8);
        used = (long)(len - 8);
    }
    if (used < 0) {
        xip_link_native_rx_abort(&buf);
        atomic_fetch_add(&rx_parse_drops, 1); atomic_fetch_add(&rx_drops, 1);
        return;
    }
    if (is_local_action_source(source)) { atomic_fetch_add(&rx_self_echoes, 1); xip_link_native_rx_abort(&buf); return; }
    if (recently_seen_action(source, buf.data, (size_t)used)) { atomic_fetch_add(&rx_duplicates, 1); xip_link_native_rx_abort(&buf); return; }
    atomic_fetch_add(&rx_actions, 1);
    xip_link_native_rx_commit(link_addr, &buf, (size_t)used, mac_token(source), 0);
}

/* The private vendor-action callback has two call forms: the vendor-reassembled one is (context, length, payload), the generic STA/AP
 * action ingress (interface, ieee80211_header, body_start, body_end). Both supply the action envelope, so both use the same body parser;
 * this is an ABI adaptation, not a raw 802.11 parser. */
static int action_rx_callback(void *peer_context, size_t second, uint8_t *third, uint8_t *fourth)
{
    if (!atomic_load(&enabled) || !peer_context || !third) return 0;
    atomic_fetch_add(&rx_dispatcher, 1);
    uint8_t source[6];
    const uint8_t *payload;
    size_t len;
    if (second <= XIP_NOW_MAX_PAYLOAD) {
        /* ieee80211_recv_action_vendor_esp_now: its first context word is the source-MAC pointer and `second` the body length */
        const uint8_t *source_ptr;
        memcpy(&source_ptr, peer_context, sizeof(source_ptr));
        if (!source_ptr) { atomic_fetch_add(&rx_invalid_drops, 1); atomic_fetch_add(&rx_drops, 1); return 0; }
        memcpy(source, source_ptr, 6);
        payload = third; len = second;
    } else {
        /* `second` is the 802.11 header address, third/fourth delimit the action body: compare the raw addresses before using them so a
         * malformed native call cannot underflow */
        const uint8_t *header = (const uint8_t *)second;
        if (header && !xip_rx_accept(header, 24)) return 0;                 /* the early filter, before the body is parsed */
        if (!header || (uintptr_t)fourth < (uintptr_t)third || (uintptr_t)fourth - (uintptr_t)third > XIP_NOW_MAX_PAYLOAD) {
            atomic_fetch_add(&rx_invalid_drops, 1); atomic_fetch_add(&rx_drops, 1);
            return 0;
        }
        memcpy(source, header + 10, 6);
        payload = third; len = (size_t)((uintptr_t)fourth - (uintptr_t)third);
    }
    if (atomic_exchange(&action_parse_busy, true)) { atomic_fetch_add(&rx_busy_drops, 1); atomic_fetch_add(&rx_drops, 1); return 0; }
    deliver(payload, len, source, false);
    atomic_store(&action_parse_busy, false);
    return 0;
}

/* ESP-IDF's action-transmit request may receive a co-channel response during its normal response interval: not remain-on-channel, the
 * request's own in-band reply hook, with the same parser and delivery as the dispatcher. */
static int action_tx_rx_callback(uint8_t *header, uint8_t *payload, size_t len, uint8_t channel)
{
    (void)channel;
    atomic_fetch_add(&rx_tx_response_hook, 1);
    if (!header || !payload || len > FRAME_CAPACITY - 24) { atomic_fetch_add(&rx_drops, 1); return 0; }
    if (atomic_exchange(&action_parse_busy, true)) { atomic_fetch_add(&rx_busy_drops, 1); atomic_fetch_add(&rx_drops, 1); return 0; }
    if (!xip_rx_accept(header, 24)) return 0;
    deliver(payload, len, header + 10, true);
    atomic_store(&action_parse_busy, false);
    return 0;
}

static void retry_ready(void *arg)
{
    (void)arg;
    if (link_addr >= 0) xip_link_native_ready(link_addr);
}

/* Ask for READY in NOW_TX_RETRY_MS. */
static void schedule_ready(void)
{
    if (!retry_timer) {
        const esp_timer_create_args_t args = {.callback = retry_ready, .dispatch_method = ESP_TIMER_TASK, .name = "espnow-ready", .skip_unhandled_events = true};
        if (esp_timer_create(&args, &retry_timer) != ESP_OK) return;
    }
    (void)esp_timer_stop(retry_timer);
    (void)esp_timer_start_once(retry_timer, NOW_TX_RETRY_MS * 1000);
}

/* ESP-IDF action submission (DMesh's transmit_submitted), serialized by tx_busy so the static request is never initialized twice at once. */
static int transmit_submitted(const uint8_t destination[6], const uint8_t *payload, size_t len)
{
    const bool broadcast = !memcmp(destination, (const uint8_t[6]){0xff, 0xff, 0xff, 0xff, 0xff, 0xff}, 6);
    if (!buffers) { atomic_fetch_add(&tx_failures, 1); return XIP_ERR_IO; }
    /* Auto normally means the STA identity; an AP-only epoch has no associated STA peer to reply from, and a client has bound its
     * connection to the AP MAC. */
    const wifi_interface_t interface = xip_wifi_ap_only_active() ? WIFI_IF_AP : WIFI_IF_STA;
    uint8_t local[6] = {0};
    (void)xip_wifi_interface_mac(interface, local);
    static const uint8_t any[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};       /* ESP-NOW-compatible actions use broadcast A3 even when associated */
    size_t frame_len = xip_now_encode(buffers->tx_frame, sizeof(buffers->tx_frame), destination, local, any, payload, len);
    if (!frame_len) { atomic_fetch_add(&tx_failures, 1); return XIP_ERR_SIZE; }
    const uint8_t *action = buffers->tx_frame + 24;
    const size_t action_len = frame_len - 24;
    uint8_t channel; wifi_second_chan_t secondary;
    if (!xip_wifi_current_channel(&channel, &secondary)) { atomic_fetch_add(&tx_failures, 1); return XIP_ERR_IO; }
    action_tx_request_t *r = &buffers->action_tx_request;
    memset(r, 0, sizeof(*r));
    r->request.ifx = interface;
    memcpy(r->request.dest_mac, destination, 6);
    r->request.type = WIFI_OFFCHAN_TX_REQ;
    r->request.channel = channel;
    r->request.sec_channel = secondary;
    r->request.wait_time_ms = NOW_ACTION_TX_WAIT_MS;
    /* a direct packet has one concrete receiver, so use the normal 802.11 ACK and retry; group-addressed records cannot be ACKed */
    r->request.no_ack = broadcast || !atomic_load(&mac_ack_enabled);
    r->request.rx_cb = action_tx_rx_callback;
    memcpy(r->request.bssid, any, 6);
    r->request.data_len = (uint32_t)action_len;
    memcpy(r->data, action, action_len);
    const esp_err_t sent = esp_wifi_action_tx_req(&r->request);
    if (sent == ESP_OK) { atomic_store(&tx_last_error, 0); atomic_fetch_add(&tx_actions, 1); return XIP_LINK_TX_DONE; }
    atomic_store(&tx_last_error, sent);
    atomic_fetch_add(&tx_failures, 1);
    return sent == ESP_ERR_NO_MEM ? XIP_ERR_BUSY : XIP_ERR_IO;
}

/* `send` to the link. `dest` is a MAC token, 0 the broadcast. XIP_LINK_TX_DONE once ESP-IDF accepted the request, XIP_ERR_BUSY when it is
 * out of memory or a transmit is in progress (READY follows), XIP_ERR_IO otherwise. */
static int link_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    (void)ctx;
    if (!atomic_load(&enabled)) return XIP_ERR_UNSUPPORTED;
    uint8_t destination[6];
    if (dest == 0) memset(destination, 0xff, 6);
    else for (int i = 0; i < 6; i++) destination[i] = (uint8_t)(dest >> (8 * i));
    if (atomic_exchange(&tx_busy, true)) { schedule_ready(); return XIP_ERR_BUSY; }
    int rc = transmit_submitted(destination, pkt->data, pkt->len);
    atomic_store(&tx_busy, false);
    if (rc == XIP_ERR_BUSY) schedule_ready();
    return rc;
}

int xip_link_espnow_start(void)
{
    if (!buffers) {
        buffers = calloc(1, sizeof(*buffers));
        if (!buffers) return XIP_ERR_NOMEM;
    }
    if (link_addr < 0) {
        link_addr = xip_link_native_register("espnow", LINK_MTU, 0, link_tx, NULL);
        if (link_addr < 0) return link_addr;
    }
    atomic_store(&enabled, true);
    /* registered after Wi-Fi is live: a stop/start of the driver loses it */
    if (ieee80211_recv_action_register(127, 0, action_rx_callback) != 0) { atomic_store(&enabled, false); return XIP_ERR_IO; }
    (void)xip_link_native_set_state(link_addr, XIP_LINK_UP, 1000);
    return XIP_OK;
}

void xip_link_espnow_stop(void)
{
    atomic_store(&enabled, false);
    if (link_addr >= 0) (void)xip_link_native_set_state(link_addr, XIP_LINK_DOWN, 0);
}
