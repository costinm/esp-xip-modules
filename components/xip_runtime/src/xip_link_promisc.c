/* Promiscuous receive as the "promisc" link, for a NAN module (the wifi component's `monitor`). The frames of the selected types are
 * delivered as pool packets: the 802.11 frame from its MAC header (radio FCS removed), the transmitter address (addr2) as the source
 * token, the RSSI as the signal. Both C and Rust hosts enter through xip_wifi_owner_monitor.
 *
 * Deliberately small: on/off and a channel. The early filter (xip_rx_filter.c) runs before a packet is taken; the runtime does not touch power save; the module decides the window (DMesh's NAN
 * owner uses 64 ms in a 512 ms discovery window), when to stop, and what to do about power save. The callback only copies. */
#include <stdatomic.h>
#include "sdkconfig.h"
#include <string.h>
#include "esp_attr.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "xip_wifi.h"

#define LINK_MTU XIP_PKT_MAX                          /* a pool packet; a longer frame is dropped */
#define MAC_HEADER 24                          /* a management frame shorter than this has no addresses */
#define FCS_LEN 4

static int link_addr = -1;
static atomic_bool on;
static atomic_uint readers;
/* Driver-only diagnostics share the existing radio debug switch. Production
 * has no resident counters; protocol diagnostics belong to the module. */
#if CONFIG_XIP_ESPNOW_DIAGNOSTICS
static struct { atomic_uint frames, drops; } stats;
#define COUNT(field) atomic_fetch_add(&stats.field, 1)
#else
#define COUNT(field) ((void)0)
#endif

static uint64_t mac_token(const uint8_t m[6])
{
    uint64_t t = 0;
    for (int i = 0; i < 6; i++) t |= (uint64_t)m[i] << (8 * i);
    return t;
}

static int no_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    (void)ctx; (void)dest; (void)pkt;
    return XIP_ERR_UNSUPPORTED;                /* receive only: frames go out as ESP-NOW actions or by the module's own tx */
}

/* the driver's receive task: copy and return, nothing else */
static void IRAM_ATTR rx_frame(void *buf, wifi_promiscuous_pkt_type_t type)
{
    (void)type;
    if (!atomic_load(&on) || !buf) return;
    const wifi_promiscuous_pkt_t *p = buf;
    size_t len = p->rx_ctrl.sig_len;
    if (len < MAC_HEADER + FCS_LEN) return;
    len -= FCS_LEN;
    if (!xip_rx_accept(p->payload, len)) return;       /* the early filter: before a pool packet is taken */
    const int32_t signal = (int32_t)(uint16_t)(int16_t)p->rx_ctrl.rssi;
    const uint32_t time_us = (uint32_t)esp_timer_get_time();
    if (xip_link_espnow_nan_promisc(p->payload, len, signal, time_us)) return;
    xip_link_rx_buf b;
    /* NAN is a protocol destination, not a promiscuous-link subscriber.
     * Classify before acquiring storage and route the original frame once. */
    const bool nan = p->payload[16] == 0x50 && p->payload[17] == 0x6f && p->payload[18] == 0x9a;
    int rc = nan ? xip_port_rx_begin_cached(XIP_PORT_NAN, &b)
                 : xip_link_native_rx_begin_cached(link_addr, &b);
    if (rc == 1) return;                       /* nobody listens */
    if (rc || len > b.capacity || len > LINK_MTU) {
        if (!rc) xip_link_native_rx_abort(&b);
        COUNT(drops);
        return;
    }
    memcpy(b.data, p->payload, len);
    COUNT(frames);
    if (nan) xip_port_rx_commit_radio(link_addr, XIP_PORT_NAN, &b, len,
        mac_token(p->payload + 10), signal, time_us);
    else xip_link_native_rx_commit(link_addr, &b, len, mac_token(p->payload + 10), signal);   /* packed signal: rssi low 16 bits, no snr */
}

static void IRAM_ATTR rx_cb(void *buf, wifi_promiscuous_pkt_type_t type)
{
    if (!atomic_load(&on)) return;
    atomic_fetch_add(&readers, 1);
    if (atomic_load(&on)) rx_frame(buf, type);
    atomic_fetch_sub(&readers, 1);
}

static void release_link(void)
{
    /* Stop admission first; an entered SDK callback must finish before its
     * link address can be reused by another provider. */
    while (atomic_load(&readers)) vTaskDelay(1);
    if (link_addr >= 0) {
        (void)xip_link_native_set_state(link_addr, XIP_LINK_DOWN, 0);
        xip_link_native_unregister(link_addr);
        link_addr = -1;
    }
}

int xip_link_promisc_set(const xip_wifi_monitor *m)
{
    if (m->channel > 13) return XIP_ERR_ARG;
    if (!m->enable) {
        atomic_store(&on, false);
        if (esp_wifi_set_promiscuous(false) != ESP_OK) return XIP_ERR_IO;
        release_link();
        return XIP_OK;
    }
    if (m->channel && !xip_wifi_associated() && !xip_wifi_set_channel(m->channel)) return XIP_ERR_IO;
    if (link_addr < 0) {
        link_addr = xip_link_native_register("promisc", LINK_MTU, 0, no_tx, NULL);
        if (link_addr < 0) return link_addr;
    }
    xip_link_native_rx_prepare();
    if (esp_wifi_set_promiscuous_rx_cb(rx_cb) != ESP_OK) { atomic_store(&on, false); release_link(); return XIP_ERR_IO; }
    atomic_store(&on, true);
    if (esp_wifi_set_promiscuous(true) != ESP_OK) { atomic_store(&on, false); release_link(); return XIP_ERR_IO; }
    (void)xip_link_native_set_state(link_addr, XIP_LINK_UP, 0);
    return XIP_OK;
}

/* the radio stops: the driver forgets promiscuous mode, so the link goes down */
void xip_link_promisc_stop(void)
{
    atomic_store(&on, false);
    (void)esp_wifi_set_promiscuous(false);
    release_link();
}

unsigned xip_link_promisc_frames(void) {
#if CONFIG_XIP_ESPNOW_DIAGNOSTICS
    return atomic_load(&stats.frames);
#else
    return 0;
#endif
}
unsigned xip_link_promisc_on(void) { return atomic_load(&on); }
