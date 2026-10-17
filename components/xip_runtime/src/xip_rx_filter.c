/* The early receive filter: fixed code in IRAM, the first thing a received frame meets, before anything is copied into a pool packet.
 * A frame is kept only when it is addressed to us or to the group (address 1, the receiver, is our MAC, our soft AP's MAC, or group-addressed:
 * broadcast, and the NAN service-discovery multicast) and belongs to a network we know (address 3 is the BSSID we are associated to, our soft AP,
 * the NAN (Aware) prefix, or the broadcast that an ESP-NOW frame carries). Our own networks are known to the runtime when it sets the radio up, so there
 * is no API for the filter; the NAN prefix is the first three bytes of a NAN cluster id (50:6f:9a), which is all that is constant in it.
 * Everything else is dropped here. Used by the promiscuous receive and by the ESP-NOW callbacks that see the 802.11 header; shared by both runtimes. */
#include <stdatomic.h>
#include <string.h>
#include "esp_attr.h"
#include "xip_wifi.h"

static _Atomic uint64_t sta_mac, ap_mac, bssid;      /* a MAC as a little-endian token; 0: none */
static atomic_uint dropped;

static inline uint64_t token(const uint8_t *m)
{
    return (uint64_t)m[0] | (uint64_t)m[1] << 8 | (uint64_t)m[2] << 16 | (uint64_t)m[3] << 24 | (uint64_t)m[4] << 32 | (uint64_t)m[5] << 40;
}
#define BROADCAST 0xffffffffffffull
#define NAN_PREFIX 0x9a6f50ull                         /* 50:6f:9a as the low three bytes of a token */

void xip_rx_filter_local(const uint8_t *sta, const uint8_t *ap)
{
    atomic_store_explicit(&sta_mac, sta ? token(sta) : 0, memory_order_relaxed);
    atomic_store_explicit(&ap_mac, ap ? token(ap) : 0, memory_order_relaxed);
}

void xip_rx_filter_bssid(const uint8_t *id)
{
    atomic_store_explicit(&bssid, id ? token(id) : 0, memory_order_relaxed);
}

/* `frame` is an 802.11 header (24 bytes at least): true to keep it. Not armed (our MAC unknown, the radio is off): keeps everything. */
bool IRAM_ATTR xip_rx_accept(const uint8_t *frame, size_t len)
{
    const uint64_t sta = atomic_load_explicit(&sta_mac, memory_order_relaxed);
    if (!sta) return true;
    if (len < 24) return false;
    const uint64_t a1 = token(frame + 4);
    if (a1 != sta && !(frame[4] & 1) && a1 != atomic_load_explicit(&ap_mac, memory_order_relaxed)) goto drop;
    const uint64_t a3 = token(frame + 16);
    if ((a3 & 0xffffff) == NAN_PREFIX) return true;                 /* NAN (Aware): cluster ids start 50:6f:9a */
    if (a3 == BROADCAST) return true;                                /* an ESP-NOW frame */
    if (a3 && a3 == atomic_load_explicit(&bssid, memory_order_relaxed)) return true;
    if (a3 && a3 == atomic_load_explicit(&ap_mac, memory_order_relaxed)) return true;
drop:
    atomic_fetch_add_explicit(&dropped, 1, memory_order_relaxed);
    return false;
}

unsigned xip_rx_filter_dropped(void) { return atomic_load(&dropped); }
