/* Promiscuous receive as the "promisc" link, for a NAN module (the wifi component's `monitor`). The frames of the selected types are
 * delivered as pool packets: the 802.11 frame from its MAC header (radio FCS removed), the transmitter address (addr2) as the source
 * token, the RSSI as the signal. The Rust twin is crates/xip-runtime-esp/src/promisc_esp.rs.
 *
 * Deliberately small: on/off and a channel. The early filter (xip_rx_filter.c) runs before a packet is taken; the runtime does not touch power save; the module decides the window (DMesh's NAN
 * owner uses 64 ms in a 512 ms discovery window), when to stop, and what to do about power save. The callback only copies. */
#include <stdatomic.h>
#include <string.h>
#include "esp_attr.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "xip_wifi.h"

#define LINK_MTU 1000                          /* a pool packet; a longer frame is dropped */
#define MAC_HEADER 24                          /* a management frame shorter than this has no addresses */
#define FCS_LEN 4

static int link_addr = -1;
static atomic_bool on;
static atomic_uint rx_frames, rx_drops;

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
static void IRAM_ATTR rx_cb(void *buf, wifi_promiscuous_pkt_type_t type)
{
    (void)type;
    if (!atomic_load(&on) || !buf) return;
    const wifi_promiscuous_pkt_t *p = buf;
    size_t len = p->rx_ctrl.sig_len;
    if (len < MAC_HEADER + FCS_LEN) return;
    len -= FCS_LEN;
    if (!xip_rx_accept(p->payload, len)) return;       /* the early filter: before a pool packet is taken */
    xip_link_rx_buf b;
    int rc = xip_link_native_rx_begin(link_addr, &b);
    if (rc == 1) return;                       /* nobody listens */
    if (rc || len > b.capacity || len > LINK_MTU) {
        if (!rc) xip_link_native_rx_abort(&b);
        atomic_fetch_add(&rx_drops, 1);
        return;
    }
    memcpy(b.data, p->payload, len);
    atomic_fetch_add(&rx_frames, 1);
    xip_link_native_rx_commit(link_addr, &b, len, mac_token(p->payload + 10), (int32_t)(uint16_t)(int16_t)p->rx_ctrl.rssi);   /* packed signal: rssi low 16 bits, no snr */
}

int xip_link_promisc_set(const xip_wifi_monitor *m)
{
    if (m->channel > 13) return XIP_ERR_ARG;
    if (!m->enable) {
        atomic_store(&on, false);
        if (esp_wifi_set_promiscuous(false) != ESP_OK) return XIP_ERR_IO;
        if (link_addr >= 0) (void)xip_link_native_set_state(link_addr, XIP_LINK_DOWN, 0);
        return XIP_OK;
    }
    if (link_addr < 0) {
        link_addr = xip_link_native_register("promisc", LINK_MTU, 0, no_tx, NULL);
        if (link_addr < 0) return link_addr;
    }
    if (m->channel && !xip_wifi_associated() && !xip_wifi_set_channel(m->channel)) return XIP_ERR_IO;
    if (esp_wifi_set_promiscuous_rx_cb(rx_cb) != ESP_OK) return XIP_ERR_IO;
    atomic_store(&on, true);
    if (esp_wifi_set_promiscuous(true) != ESP_OK) { atomic_store(&on, false); return XIP_ERR_IO; }
    (void)xip_link_native_set_state(link_addr, XIP_LINK_UP, 0);
    return XIP_OK;
}

/* the radio stops: the driver forgets promiscuous mode, so the link goes down */
void xip_link_promisc_stop(void)
{
    if (!atomic_exchange(&on, false)) return;
    (void)esp_wifi_set_promiscuous(false);
    if (link_addr >= 0) (void)xip_link_native_set_state(link_addr, XIP_LINK_DOWN, 0);
}

unsigned xip_link_promisc_frames(void) { return atomic_load(&rx_frames); }
unsigned xip_link_promisc_on(void) { return atomic_load(&on); }
