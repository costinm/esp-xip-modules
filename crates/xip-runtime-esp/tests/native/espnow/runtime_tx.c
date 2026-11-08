/* Actual runtime TX/return functions against a real pool; only registry/inbox
 * and native transport boundaries are faked. Generated include comes from C
 * source at test time so these tests cannot silently test a copied algorithm. */
#include <assert.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "esp_timer.h"
#include "xip_runtime.h"
#define XIP_LINKS_MAX 1
struct xip_instance { int component; uint32_t port; struct { unsigned pkt_sent, pkt_dropped; } stats; };
#define XIP_PORTS_MAX 8
static struct { uint32_t port; int component; } ports[XIP_PORTS_MAX];
typedef struct { bool used, wait_readers; unsigned provider; int owner, tx_component, tx_method; xip_link_tx_fn tx; void *tx_ctx; unsigned sent, dropped, forwarded, key_len, key_offset, nroutes; struct { uint8_t key[16]; uint32_t egress; uint64_t token; uint32_t flags; } routes[1]; } link_t;
static struct xip_instance sender, provider;
static xip_pool *pool;
static portMUX_TYPE registry_lock = portMUX_INITIALIZER_UNLOCKED;
static link_t links[1];
#define link links[0]
static unsigned ready_events;
static uint32_t link_addr(const link_t *l) { (void)l; return XIP_ADDR_LINK; }
static void link_event(uint32_t addr, uint32_t kind, int32_t value, uint64_t id) { (void)value; (void)id; assert(addr == XIP_ADDR_LINK && kind == XIP_LINK_EVENT_READY); ready_events++; }
static uint32_t sending_handle;
static int inbox_slot = -1, inbox_owner, inbox_outcome, native_result, native_calls;
static struct xip_instance *I(void *ctx) { return ctx; }
static struct xip_instance *by_component(int c, const struct xip_instance *except) {
    (void)except; return c == sender.component ? &sender : c == provider.component ? &provider : NULL;
}
static link_t *link_at_locked(uint32_t addr) { return addr == XIP_ADDR_LINK ? &link : NULL; }
static int deliver(struct xip_instance *t, int slot, uint16_t start, uint16_t len, int component, int method, uint32_t from, uint64_t token, int32_t outcome, uint32_t flags, uint32_t time, const void *src) {
    (void)start; (void)len; (void)component; (void)method; (void)from; (void)token; (void)flags; (void)time; (void)src;
    assert(inbox_slot < 0 && xip_pool_refs(pool, slot) == 1);
    inbox_slot = slot; inbox_owner = t->component; inbox_outcome = outcome; return 0;
}
#include "runtime_tx.inc"
static int native_tx(void *ctx, uint64_t dest, const xip_link_packet *packet) {
    (void)ctx; (void)dest; native_calls++;
    assert(xip_pool_refs(packet->pool, packet->slot)>=1);
    assert(packet->len == 4 && !memcmp(packet->data, "ping", 4));
    return native_result;
}
static int packet(void) {
    int slot = xip_pool_acquire(pool, 0); assert(slot >= 0);
    sending_handle = xip_packet_handle(pool,slot,XIP_PKT_HEADROOM,4);
    assert(sending_handle); memcpy(xip_pool_slot(pool, slot) + XIP_PKT_HEADROOM, "ping", 4); return slot;
}
static void release_inbox(void) { assert(inbox_slot >= 0); drop_slot(inbox_slot); inbox_slot = -1; }
void test_runtime_ownership(void) {
    memset(&sender, 0, sizeof(sender)); memset(&provider, 0, sizeof(provider));
    sender.component = 20; provider.component = 30;
    sender.port = provider.port = 3;
    for (unsigned run = 0; run < 24; run++) {
        assert(port_bind(&sender) == XIP_OK && port_bind(&sender) == XIP_OK);
        unsigned held = 0;
        for (unsigned i = 0; i < XIP_PORTS_MAX; i++) held += ports[i].port != 0;
        assert(held == 1); /* repeated bind is idempotent, not duplicate fan-out */
        assert(port_bind(&provider) == XIP_ERR_BUSY);
        serial_release(&provider);
        assert(ports[0].component == sender.component); /* another owner cannot release it */
        serial_release(&sender);
        assert(port_bind(&provider) == XIP_OK);
        serial_release(&provider);
    }
    pool=xip_pool_create(3,XIP_PKT_SLOT_MIN);assert(pool);xip_pool_prepare_rx(pool);
    link=(link_t){.used=true,.provider=XIP_LINK_NATIVE,.tx=native_tx};
    int slot=packet();xip_pool_retain(pool,slot);native_result=XIP_ERR_BUSY;
    assert(link_send(&sender,sending_handle,4,XIP_ADDR_LINK,0,0,0)==XIP_ERR_BUSY);
    assert(xip_pool_refs(pool,slot)==2 && !memcmp(xip_pool_slot(pool,slot)+XIP_PKT_HEADROOM,"ping",4));
    drop_slot(slot);native_result=XIP_LINK_TX_DONE;
    assert(link_send(&sender,sending_handle,4,XIP_ADDR_LINK,0,0,0)==0);
    assert(!xip_packet_get(pool,sending_handle) && xip_pool_available(pool)==3);
    slot=packet();native_result=0;
    assert(link_send(&sender,sending_handle,4,XIP_ADDR_LINK,20,7,0)==0);
    assert(xip_pool_refs(pool,slot)==1 && inbox_slot<0);drop_slot(slot);
    link.provider=XIP_LINK_MODULE;link.owner=provider.component;
    slot=packet();assert(link_send(&sender,sending_handle,4,XIP_ADDR_LINK,20,7,0)==0);
    assert(inbox_slot==slot && inbox_owner==30);inbox_slot=-1;
    assert(c_link_tx_done(&provider,XIP_ADDR_LINK,sending_handle,XIP_OK)==0);
    assert(xip_pool_available(pool)==3 && inbox_slot<0);
    slot=packet();native_result=XIP_ERR_IO;link.provider=XIP_LINK_NATIVE;
    assert(link_send(&sender,sending_handle,4,XIP_ADDR_LINK,20,7,0)==XIP_ERR_IO);
    assert(xip_pool_available(pool)==3);
    /* Explicit route COPY clones into separate headroom; a native driver still
     * receives one unique lease, while observers retain the original. */
    native_result = XIP_LINK_TX_DONE;
    link.key_len = 1; link.nroutes = 1; link.routes[0].key[0] = 'p';
    link.routes[0].egress = XIP_ADDR_LINK; link.routes[0].flags = XIP_ROUTE_COPY;
    slot = packet(); 
    assert(!link_route(XIP_ADDR_LINK, slot, XIP_PKT_HEADROOM, 4));
    assert(xip_pool_refs(pool, slot) == 1 && !memcmp(xip_pool_slot(pool, slot) + XIP_PKT_HEADROOM, "ping", 4));
    drop_slot(slot);
    slot = packet(); 
    int other = xip_pool_acquire_cached(pool, 0); assert(other >= 0);
    int before = native_calls;
    assert(!link_route(XIP_ADDR_LINK, slot, XIP_PKT_HEADROOM, 4));
    assert(native_calls == before && xip_pool_allocated(pool) == 2); /* no RX malloc */
    drop_slot(other); drop_slot(slot);
    assert(xip_pool_available(pool) == 3);
    xip_pool_destroy(pool); pool = NULL;
}
