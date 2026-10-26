/* The runtime's UDP multicast listener. It is a native bearer, not a generic
 * multicast facility: one IPv6 socket bound to the DMesh discovery port,
 * joined to the link-local discovery group, that hands each received datagram
 * to XIP_PORT_MC (the module whose manifest key 9 binds it) and answers to the
 * source it arrived from. IPv4 is a stub until the host sweeps it. It is on
 * while there is an IP (xip_connected); with no IP there is no UDP, and
 * discovery falls back to NOW, which needs none. */
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <stdatomic.h>
#include "sdkconfig.h"
#include "esp_netif.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "lwip/sockets.h"
#include "lwip/netif.h"
#include "xip_runtime.h"
#include "xip_lwip_io.h"
#include "xip_udp_peers.h"
#include "internal.h"

/* IPv6: host (dmesh_server::discovery) sends ff02::5227:5227. Keep the group
 * and port in lockstep with that sweep; IPv4 stays a stub until it has a group. */
#define XIP_MC_PORT 5227
static const uint8_t XIP_MC_GROUP6[16] = {
    0xff, 0x02,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0x52, 0x27
};

struct mc_state {
    int fd, link;
    xip_io_registration watch;
    _Atomic unsigned refs;
    xip_udp_peers peers;
};
static struct mc_state *state;
static SemaphoreHandle_t mutex;
static uint32_t epoch_sequence;

static void mc_release(struct mc_state *s) { atomic_fetch_sub(&s->refs, 1); }
static struct mc_state *mc_acquire(uint32_t epoch, xip_io_registration reg, int fd)
{
    if (!mutex) return NULL;
    xSemaphoreTake(mutex, portMAX_DELAY);
    struct mc_state *s = state;
    if (s && ((!epoch || s->peers.epoch == epoch) && (!reg || (s->watch == reg && s->fd == fd))))
        atomic_fetch_add(&s->refs, 1);
    else
        s = NULL;
    xSemaphoreGive(mutex);
    return s;
}

/* Answer to a source token the request arrived with: unicast back to the host. */
static int mc_tx(void *ctx, uint64_t dest, const xip_link_packet *pkt)
{
    struct mc_state *s = mc_acquire((uint32_t)(uintptr_t)ctx, 0, 0);
    if (!s) return XIP_ERR_NOTFOUND;
    xip_udp_address a;
    xSemaphoreTake(mutex, portMAX_DELAY);
    bool found = state == s && xip_udp_peer_get(&s->peers, dest, (uint64_t)esp_timer_get_time(), &a);
    xSemaphoreGive(mutex);
    if (!found) { mc_release(s); return XIP_ERR_NOTFOUND; }
    struct sockaddr_in6 to = {.sin6_family = AF_INET6, .sin6_port = htons(a.port), .sin6_scope_id = a.scope};
    memcpy(&to.sin6_addr, a.ip, 16);
    int n = sendto(s->fd, pkt->data, pkt->len, MSG_DONTWAIT, (struct sockaddr *)&to, sizeof(to));
    int rc = n == (int)pkt->len ? XIP_LINK_TX_DONE : (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) ? XIP_ERR_BUSY : XIP_ERR_IO;
    mc_release(s);
    return rc;
}

static void mc_ready(xip_io_registration reg, int fd, int16_t events, void *ctx)
{
    (void)ctx;
    if (!(events & POLLIN)) return;
    struct mc_state *s = mc_acquire(0, reg, fd);
    if (!s) return;
    for (unsigned i = 0; i < 4; i++) {
        struct sockaddr_in6 from = {0};
        socklen_t flen = sizeof(from);
        xip_link_rx_buf buf;
        /* The discovery port must be bound (a module listening) or the buffer
         * acquire fails; drop the datagram when nobody is there for it. */
        if (xip_port_rx_begin(XIP_PORT_MC, &buf) != 0) {
            uint8_t scratch;
            (void)recvfrom(fd, &scratch, 1, MSG_DONTWAIT, (struct sockaddr *)&from, &flen);
            break;
        }
        int n = recvfrom(fd, buf.data, buf.capacity, MSG_DONTWAIT, (struct sockaddr *)&from, &flen);
        if (n <= 0) { xip_port_rx_abort(&buf); break; }
        xip_udp_address a = {.port = ntohs(from.sin6_port), .scope = from.sin6_scope_id};
        memcpy(a.ip, &from.sin6_addr, 16);
        uint64_t token = 0;
        xSemaphoreTake(mutex, portMAX_DELAY);
        if (state == s && from.sin6_family == AF_INET6)
            token = xip_udp_peer_note(&s->peers, &a, (uint64_t)esp_timer_get_time());
        xSemaphoreGive(mutex);
        if (!token) { xip_port_rx_abort(&buf); continue; }
        /* Deliver to the discovery port; the module answers via the source token. */
        xip_port_rx_commit(s->link, XIP_PORT_MC, &buf, (size_t)n, token);
    }
    mc_release(s);
}

void xip_mc_stop(void)
{
    if (!mutex) return;
    xSemaphoreTake(mutex, portMAX_DELAY);
    struct mc_state *s = state;
    state = NULL;
    xSemaphoreGive(mutex);
    if (!s) return;
    xip_io_clear(s->watch);
    xip_link_native_set_state(s->link, XIP_LINK_DOWN, 0);
    while (atomic_load(&s->refs) != 1) vTaskDelay(1);
    close(s->fd);
    xip_link_native_unregister(s->link);
    free(s);
}

unsigned xip_mc_sockets(void)
{
    if (!mutex) return 0;
    xSemaphoreTake(mutex, portMAX_DELAY);
    unsigned n = state ? 1 : 0;
    xSemaphoreGive(mutex);
    return n;
}

void xip_mc_ensure(void)
{
    if (!xip_connected()) return;
    if (!mutex) mutex = xSemaphoreCreateMutex();
    if (!mutex) return;
    xSemaphoreTake(mutex, portMAX_DELAY);
    if (state) { xSemaphoreGive(mutex); return; }
    lwip_socket_thread_init();
    int fd = socket(AF_INET6, SOCK_DGRAM, 0);
    if (fd < 0) { xSemaphoreGive(mutex); return; }
    int on = 1;
    (void)setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on));
    int off = 0;
    (void)setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &off, sizeof(off));
    struct sockaddr_in6 any = {.sin6_family = AF_INET6, .sin6_port = htons(XIP_MC_PORT), .sin6_addr = IN6ADDR_ANY_INIT};
    if (bind(fd, (struct sockaddr *)&any, sizeof(any)) != 0) { close(fd); xSemaphoreGive(mutex); return; }
    /* Join the link-local discovery group on the STA netif (interface 0 = the
     * default netif, which for a single-interface device is the Wi-Fi STA). */
    struct ipv6_mreq mreq = {0};
    memcpy(&mreq.ipv6mr_multiaddr, XIP_MC_GROUP6, 16);
    mreq.ipv6mr_interface = 0;
    (void)setsockopt(fd, IPPROTO_IPV6, IPV6_JOIN_GROUP, &mreq, sizeof(mreq));
    if (fcntl(fd, F_SETFL, O_NONBLOCK) < 0) { close(fd); xSemaphoreGive(mutex); return; }
    struct mc_state *s = calloc(1, sizeof(*s));
    if (!s) { close(fd); xSemaphoreGive(mutex); return; }
    s->fd = fd;
    s->peers.epoch = ++epoch_sequence;
    if (!s->peers.epoch) s->peers.epoch = ++epoch_sequence;
    atomic_init(&s->refs, 1);
    s->link = xip_link_native_register("mc", XIP_PKT_MAX, 0, mc_tx, (void *)(uintptr_t)s->peers.epoch);
    if (s->link < 0) { free(s); close(fd); xSemaphoreGive(mutex); return; }
    state = s;
    s->watch = xip_io_watch(fd, POLLIN, mc_ready, NULL);
    if (!s->watch) {
        state = NULL;
        xip_link_native_unregister(s->link);
        close(fd);
        free(s);
    } else {
        xip_link_native_set_state(s->link, XIP_LINK_UP, 0);
    }
    xSemaphoreGive(mutex);
}
