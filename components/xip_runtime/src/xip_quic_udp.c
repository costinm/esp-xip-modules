/* Native default UDP bearer: exact reversible peers, shared pool and polling.
 * RX commit runs outside the socket lock: it may synchronously relay to UDP. */
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include "sdkconfig.h"
#include "esp_netif.h"
#include "esp_wifi.h"
#include "esp_private/wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "lwip/sockets.h"
#include "xip_runtime.h"
#include "xip_lwip_io.h"
#include "xip_udp_peers.h"
#include "internal.h"
#if CONFIG_XIP_QUIC_UDP
/* BEGIN UDP6 CODEC: host parity tests compile this exact implementation. */
/* Ethernet/IPv6/UDP subset of fw/transport/src/raw_udp6.rs.
 * Plain unfragmented UDP only; lwIP retains NDP and every other protocol. */
#define XIP_UDP6_PREFIX 62u
#define XIP_UDP6_PAYLOAD_MAX 1452u
static inline uint16_t udp6_get16(const uint8_t *p) { return ((uint16_t)p[0]<<8)|p[1]; }
static inline void udp6_put16(uint8_t *p,uint16_t n) { p[0]=n>>8;p[1]=n; }
static inline void udp6_local_ip(const uint8_t mac[6],uint8_t ip[16])
{
    memset(ip,0,16);ip[0]=0xfe;ip[1]=0x80;
    ip[8]=mac[0]^2;ip[9]=mac[1];ip[10]=mac[2];ip[11]=0xff;
    ip[12]=0xfe;memcpy(ip+13,mac+3,3);
}
static inline uint32_t udp6_sum(uint32_t sum,const uint8_t *p,size_t n)
{
    while(n>=2){sum+=udp6_get16(p);p+=2;n-=2;}
    if(n)sum+=(uint16_t)*p<<8;
    return sum;
}
static inline uint16_t udp6_checksum(const uint8_t *src,const uint8_t *dst,const uint8_t *udp,size_t n)
{
    uint32_t sum=udp6_sum(udp6_sum(0,src,16),dst,16)+(uint32_t)n+17;
    sum=udp6_sum(sum,udp,n);
    while(sum>>16)sum=(sum&65535)+(sum>>16);
    return (uint16_t)~sum;
}
/* Classification precedes validation. A selected but malformed datagram is
 * dropped, never delivered a second time through the socket path. */
static inline bool udp6_selected(const uint8_t *f,size_t n,const uint8_t local[16],uint16_t port)
{
    return n>=XIP_UDP6_PREFIX && udp6_get16(f+12)==0x86dd && f[14]>>4==6
        && f[20]==17 && !memcmp(f+38,local,16) && udp6_get16(f+56)==port;
}
static inline bool udp6_valid(const uint8_t *f,size_t n)
{
    if(n<XIP_UDP6_PREFIX)return false;
    size_t len=udp6_get16(f+18);
    return len>=8 && len<=8+XIP_UDP6_PAYLOAD_MAX && len<=n-54
        && udp6_get16(f+58)==len && udp6_get16(f+54)!=0
        && udp6_get16(f+60)!=0 && udp6_checksum(f+22,f+38,f+54,len)==0;
}
/* Only prefix bytes are written. Payload already occupies its owned lease. */
static inline void udp6_prefix(uint8_t *f,size_t n,const uint8_t src_mac[6],const uint8_t dst_mac[6],
                               const uint8_t src_ip[16],const uint8_t dst_ip[16],uint16_t src_port,uint16_t dst_port)
{
    memcpy(f,dst_mac,6);memcpy(f+6,src_mac,6);udp6_put16(f+12,0x86dd);
    memset(f+14,0,48);f[14]=0x60;udp6_put16(f+18,(uint16_t)(n+8));f[20]=17;f[21]=64;
    memcpy(f+22,src_ip,16);memcpy(f+38,dst_ip,16);
    udp6_put16(f+54,src_port);udp6_put16(f+56,dst_port);udp6_put16(f+58,(uint16_t)(n+8));
    uint16_t sum=udp6_checksum(src_ip,dst_ip,f+54,n+8);udp6_put16(f+60,sum?sum:65535);
}
/* END UDP6 CODEC */
struct raw_peer {uint64_t token; uint8_t mac[6],local_mac[6],local_ip[16]; wifi_interface_t interface; bool valid;};
static _Atomic bool raw_mode;
void xip_quic_udp_raw_mode(bool enabled){atomic_store(&raw_mode,enabled);}
struct udp_state {int fd,link;xip_io_registration watch;_Atomic unsigned refs; xip_udp_peers peers; struct raw_peer *raw;};
static struct udp_state *state;
static SemaphoreHandle_t mutex;
static uint32_t epoch_sequence;
static void udp_release(struct udp_state *s){atomic_fetch_sub(&s->refs,1);}
static struct udp_state *udp_acquire(uint32_t epoch,xip_io_registration reg,int fd)
{
    if(!mutex)return NULL;
    xSemaphoreTake(mutex,portMAX_DELAY);struct udp_state *s=state;
    if(s && ((!epoch || s->peers.epoch==epoch) && (!reg || (s->watch==reg && s->fd==fd))))atomic_fetch_add(&s->refs,1);
    else s=NULL;
    xSemaphoreGive(mutex);return s;
}
int xip_quic_udp_destination(const uint8_t address[22],uint64_t *token)
{
    if(!address || !token)return XIP_ERR_ARG;
    xip_quic_udp_ensure();
    if(!mutex)return XIP_ERR_NOTFOUND;
    xip_udp_address a={.port=((uint16_t)address[16]<<8)|address[17],
        .scope=((uint32_t)address[18]<<24)|((uint32_t)address[19]<<16)|((uint32_t)address[20]<<8)|address[21]};
    if(!a.port)return XIP_ERR_ARG;
    memcpy(a.ip,address,16);
    xSemaphoreTake(mutex,portMAX_DELAY);
    *token=state?xip_udp_peer_note(&state->peers,&a,(uint64_t)esp_timer_get_time()):0;
    xSemaphoreGive(mutex);return *token?0:XIP_ERR_NOMEM;
}
static int udp_tx(void *ctx,uint64_t dest,const xip_link_packet *pkt)
{
    struct udp_state *s=udp_acquire((uint32_t)(uintptr_t)ctx,0,0);
    if(!s)return XIP_ERR_NOTFOUND;
    xip_udp_address a;
    xSemaphoreTake(mutex,portMAX_DELAY);
    struct raw_peer raw={0};
    bool found=state==s && xip_udp_peer_get(&s->peers,dest,(uint64_t)esp_timer_get_time(),&a);
    if(found && s->raw && s->raw[(dest&255)-1].token==dest)raw=s->raw[(dest&255)-1];
    xSemaphoreGive(mutex);
    if(!found){udp_release(s);return XIP_ERR_NOTFOUND;}
    if(raw.valid){
        if(pkt->headroom<XIP_UDP6_PREFIX || pkt->len>XIP_UDP6_PAYLOAD_MAX){udp_release(s);return XIP_ERR_SIZE;}
        uint8_t *frame=pkt->data-XIP_UDP6_PREFIX;
        udp6_prefix(frame,pkt->len,raw.local_mac,raw.mac,raw.local_ip,a.ip,CONFIG_XIP_QUIC_UDP_PORT,a.port);
        /* The SDK tx_by_ref uses lwIP's globally registered pbuf ref/free hooks,
         * not our packet leases. internal_tx copies; retain ownership until it
         * returns. No driver-referencing lease escapes this synchronous call. */
        int err=esp_wifi_internal_tx(raw.interface,frame,pkt->len+XIP_UDP6_PREFIX);
        int rc=err==ESP_OK?XIP_LINK_TX_DONE:err==ESP_ERR_NO_MEM?XIP_ERR_BUSY:XIP_ERR_IO;
        udp_release(s);return rc;
    }
    struct sockaddr_in6 to={.sin6_family=AF_INET6,.sin6_port=htons(a.port),.sin6_scope_id=a.scope};
    memcpy(&to.sin6_addr,a.ip,16);
    /* lwIP's public socket API owns its unavoidable SDK copy. There is no
     * send-by-reference API here; the module keeps its exact pool lease until DONE. */
    int n=sendto(s->fd,pkt->data,pkt->len,MSG_DONTWAIT,(struct sockaddr *)&to,sizeof(to));
    int rc=n==(int)pkt->len?XIP_LINK_TX_DONE:n<0 && (errno==EAGAIN || errno==EWOULDBLOCK)?XIP_ERR_BUSY:XIP_ERR_IO;
    udp_release(s);return rc;
}
static void udp_ready(xip_io_registration reg,int fd,int16_t events,void *ctx)
{
    (void)ctx;if(!(events&POLLIN))return;
    struct udp_state *s=udp_acquire(0,reg,fd);if(!s)return;
    for(unsigned i=0;i<4;i++){
        struct sockaddr_in6 from={0};socklen_t flen=sizeof(from);xip_link_rx_buf buf;
        if(xip_link_native_rx_begin(s->link,&buf)!=0){uint8_t scratch;(void)recvfrom(fd,&scratch,1,MSG_DONTWAIT,(struct sockaddr *)&from,&flen);break;}
        int n=recvfrom(fd,buf.data,buf.capacity,MSG_DONTWAIT,(struct sockaddr *)&from,&flen);
        if(n<=0){xip_link_native_rx_abort(&buf);break;}
        xip_udp_address a={.port=ntohs(from.sin6_port),.scope=from.sin6_scope_id};memcpy(a.ip,&from.sin6_addr,16);
        xSemaphoreTake(mutex,portMAX_DELAY);
        uint64_t token=state==s && from.sin6_family==AF_INET6 ? xip_udp_peer_note(&s->peers,&a,(uint64_t)esp_timer_get_time()):0;
        xSemaphoreGive(mutex);
        if(!token){xip_link_native_rx_abort(&buf);continue;}
        /* No socket mutex held across classifier/relay/native egress. Epoch ref
         * keeps this link and fd alive until dispatch finishes. */
        xip_link_native_rx_commit(s->link,&buf,(size_t)n,token,0);
    }
    udp_release(s);
}
/* Called by the Wi-Fi owner's Ethernet callback. No heap growth, semantic
 * dispatch or blocking socket operations on the SDK task. The SDK owns frame. */
static bool mc_raw_rx(unsigned interface,const uint8_t *frame,size_t len);
bool xip_quic_udp_raw_rx(unsigned interface,const uint8_t *frame,size_t len)
{
    if(!atomic_load(&raw_mode) || !mutex || xSemaphoreTake(mutex,0)!=pdTRUE)return false;
    struct udp_state *s=state;
    if(!s || !s->raw){xSemaphoreGive(mutex);return false;}
    atomic_fetch_add(&s->refs,1);xSemaphoreGive(mutex);
    uint8_t mac[6],ip[16];
    if(esp_wifi_get_mac((wifi_interface_t)interface,mac)!=ESP_OK){udp_release(s);return false;}
    udp6_local_ip(mac,ip);
    /* The raw path bypasses lwIP for two ingress classes only, both must fall
     * through (return false) to lwIP's socket listeners for everything else:
     * 1. the QUIC UDP port, addressed to this interface's own link-local
     *    (the raw QUIC bearer), and
     * 2. the discovery multicast wire port (XIP_MC_UDP_PORT): the mc socket's
     *    RX is unreliable under lwIP's poll on this composition, while the raw
     *    ingress is proven. The mc socket remains the TX reply path and its
     *    MLD join is what admits the group's frames to this callback. */
    if(!udp6_selected(frame,len,ip,CONFIG_XIP_QUIC_UDP_PORT)){
        udp_release(s);
        return mc_raw_rx(interface,frame,len);
    }
    if(!udp6_valid(frame,len)){udp_release(s);return true;}
    xip_link_rx_buf buf;
    if(xip_link_native_rx_begin_cached(s->link,&buf)!=0){udp_release(s);return true;}
    xip_udp_address a={.port=udp6_get16(frame+54),.scope=interface+1};memcpy(a.ip,frame+22,16);
    uint64_t token=0;
    if(xSemaphoreTake(mutex,0)==pdTRUE){
        if(state==s)token=xip_udp_peer_note(&s->peers,&a,(uint64_t)esp_timer_get_time());
        if(token){struct raw_peer *r=&s->raw[(token&255)-1];
            memcpy(r->mac,frame+6,6);memcpy(r->local_mac,mac,6);memcpy(r->local_ip,ip,16);
            r->token=token;r->interface=(wifi_interface_t)interface;r->valid=true;}
        xSemaphoreGive(mutex);
    }
    if(!token)xip_link_native_rx_abort(&buf);
    else {
        size_t n=udp6_get16(frame+58)-8;
        /* One unavoidable SDK RX -> pool copy; ownership transfers at commit. */
        memcpy(buf.data,frame+XIP_UDP6_PREFIX,n);
        xip_link_native_rx_commit(s->link,&buf,n,token,0);
    }
    udp_release(s);return true;
}
void xip_quic_udp_stop(void)
{
    if(!mutex)return;
    xSemaphoreTake(mutex,portMAX_DELAY);struct udp_state *s=state;state=NULL;xSemaphoreGive(mutex);
    if(!s)return;
    xip_io_clear(s->watch);xip_link_native_set_state(s->link,XIP_LINK_DOWN,0);
    /* These refs cover bounded nonblocking socket callbacks only, never module
     * handlers. Yield so a lower-priority polling task can finish its batch.
     * Do not reuse/unregister the link while old-epoch dispatch can still run. */
    while(atomic_load(&s->refs)!=1)vTaskDelay(1);
    close(s->fd);xip_link_native_unregister(s->link);free(s->raw);free(s);
}
unsigned xip_quic_udp_sockets(void)
{
    if(!mutex)return 0;
    xSemaphoreTake(mutex,portMAX_DELAY);unsigned n=state?1:0;xSemaphoreGive(mutex);return n;
}
void xip_quic_udp_ensure(void)
{
    if(!xip_connected())return;
    if(!mutex)mutex=xSemaphoreCreateMutex();
    if(!mutex)return;
    xSemaphoreTake(mutex,portMAX_DELAY);if(state){xSemaphoreGive(mutex);return;}
    lwip_socket_thread_init();int fd=socket(AF_INET6,SOCK_DGRAM,0);if(fd<0){xSemaphoreGive(mutex);return;}
    int off=0;(void)setsockopt(fd,IPPROTO_IPV6,IPV6_V6ONLY,&off,sizeof(off));
    struct sockaddr_in6 any={.sin6_family=AF_INET6,.sin6_port=htons(CONFIG_XIP_QUIC_UDP_PORT),.sin6_addr=IN6ADDR_ANY_INIT};
    if(fcntl(fd,F_SETFL,O_NONBLOCK)<0 || bind(fd,(struct sockaddr *)&any,sizeof(any))!=0){close(fd);xSemaphoreGive(mutex);return;}
    struct udp_state *s=calloc(1,sizeof(*s));if(!s){close(fd);xSemaphoreGive(mutex);return;}
    if(atomic_load(&raw_mode)){
        s->raw=calloc(XIP_UDP_PEERS,sizeof(*s->raw));
        if(!s->raw){free(s);close(fd);xSemaphoreGive(mutex);return;}
    }
    s->fd=fd;s->peers.epoch=++epoch_sequence;if(!s->peers.epoch)s->peers.epoch=++epoch_sequence;atomic_init(&s->refs,1);
    s->link=xip_link_native_register("udp",s->raw?XIP_UDP6_PAYLOAD_MAX:XIP_PKT_MAX,0,udp_tx,(void *)(uintptr_t)s->peers.epoch);
    if(s->link<0){free(s->raw);free(s);close(fd);xSemaphoreGive(mutex);return;}
    xip_link_native_rx_prepare(); /* Prepare before SDK callbacks; no callback-side pool initialization. */
    state=s;s->watch=xip_io_watch(fd,POLLIN,udp_ready,NULL);
    if(!s->watch){state=NULL;xip_link_native_unregister(s->link);close(fd);free(s->raw);free(s);}
    else xip_link_native_set_state(s->link,XIP_LINK_UP,0);
    xSemaphoreGive(mutex);
}
#else
int xip_quic_udp_destination(const uint8_t address[22],uint64_t *token){(void)address;(void)token;return XIP_ERR_UNSUPPORTED;}
void xip_quic_udp_raw_mode(bool enabled){(void)enabled;}
bool xip_quic_udp_raw_rx(unsigned interface,const uint8_t *frame,size_t len){(void)interface;(void)frame;(void)len;return false;}
void xip_quic_udp_ensure(void){}
void xip_quic_udp_stop(void){}
unsigned xip_quic_udp_sockets(void){return 0;}
#endif

/* ===== The magic discovery port (the DMesh sweep dst): the runtime's mc UDP socket =====
 * The same runtime UDP port lifecycle as QUIC's engine socket (opened together on the
 * NETWORK event, stopped at wifi down, counted with it), nothing special beyond the
 * magic port number and its multicast join. Not a link: each datagram is delivered to
 * the port's module with its source (the call's `src`) and the socket handle on
 * `call.from`; the module answers with `udp_send` on the handle. No peers table, no
 * private queue; the port begin/commit drop semantics are the serial port's. */
/* IPv6: host (dmesh_server::discovery) sends ff02::5227. Lockstep with the sweep. */
static const uint8_t XIP_MC_GROUP6[16] = {
    0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x52, 0x27
};
struct mc_state {int fd;xip_io_registration watch;_Atomic unsigned refs;};
static struct mc_state *mc_state;
static SemaphoreHandle_t mc_mutex;
static void mc_release(struct mc_state *s){atomic_fetch_sub(&s->refs,1);}
static struct mc_state *mc_take(void)
{
    if(!mc_mutex)return NULL;
    xSemaphoreTake(mc_mutex,portMAX_DELAY);
    struct mc_state *s=mc_state;
    if(s)atomic_fetch_add(&s->refs,1);
    xSemaphoreGive(mc_mutex);
    return s;
}
static bool mc_raw_rx(unsigned interface,const uint8_t *frame,size_t len);
static struct mc_state *mc_acquire(xip_io_registration reg,int fd)
{
    if(!mc_mutex)return NULL;
    xSemaphoreTake(mc_mutex,portMAX_DELAY);
    struct mc_state *s=mc_state;
    if(s && s->watch==reg && s->fd==fd)atomic_fetch_add(&s->refs,1);
    else s=NULL;
    xSemaphoreGive(mc_mutex);
    return s;
}
static void mc_ready(xip_io_registration reg,int fd,int16_t events,void *ctx);
static int udp_reply_fd(int fallback);
static void mc_ready(xip_io_registration reg,int fd,int16_t events,void *ctx)
{
    (void)ctx;
    if(!(events & POLLIN))return;
    struct mc_state *s=mc_acquire(reg,fd);
    if(!s)return;
    for(unsigned i=0;i<4;i++){
        struct sockaddr_in6 from={0};socklen_t flen=sizeof(from);xip_link_rx_buf buf;
        if(xip_port_rx_begin(XIP_PORT_MC,&buf)!=0){uint8_t scratch;(void)recvfrom(fd,&scratch,1,MSG_DONTWAIT,(struct sockaddr *)&from,&flen);break;}
        int n=recvfrom(fd,buf.data,buf.capacity,MSG_DONTWAIT,(struct sockaddr *)&from,&flen);
        if(n<=0){xip_port_rx_abort(&buf);break;}
        if(from.sin6_family!=AF_INET6){xip_port_rx_abort(&buf);continue;}
        /* The handle the module received on `call.from` and answers on. The
         * reply must originate from the QUIC engine's port — the device's
         * fixed UDP port — so a peer that discovered us dials its association
         * to the port that actually answers QUIC. Fall back to this socket
         * only while the engine is down (no association can be dialled then). */
        uint32_t handle=XIP_ADDR_UDP+(uint32_t)udp_reply_fd(fd);
        xip_udp_address src={.port=ntohs(from.sin6_port),.scope=from.sin6_scope_id};
        memcpy(src.ip,&from.sin6_addr,16);
        /* Unlocked commit: dispatch may send through this file's `h_udp_send`. */
        xip_port_rx_commit_udp(handle,XIP_PORT_MC,&buf,(size_t)n,&src);
    }
    mc_release(s);
}
/* Raw multicast ingress for the discovery port: same wire result as the socket
 * path (`mc_ready`), one frame at a time off the Wi-Fi SDK task — pool
 * acquire/copy/commit only, like the QUIC raw path. */
static bool mc_raw_rx(unsigned interface,const uint8_t *frame,size_t len)
{
    struct mc_state *s=mc_take();
    if(!s)return false; /* Socket down: lwIP owns the frame. */
    bool consumed=false;
    if(len>=XIP_UDP6_PREFIX && !memcmp(frame+38,XIP_MC_GROUP6,16) &&
       udp6_get16(frame+56)==XIP_MC_UDP_PORT && udp6_valid(frame,len)){
        xip_link_rx_buf buf;
        /* Pool-short still consumes: lwIP's socket path needs the same pool. */
        if(xip_port_rx_begin(XIP_PORT_MC,&buf)==0){
            size_t n=len-XIP_UDP6_PREFIX;
            if(n<=(size_t)buf.capacity && n<=XIP_PKT_MAX){
                memcpy(buf.data,frame+XIP_UDP6_PREFIX,n);
                xip_udp_address src={.port=udp6_get16(frame+54),.scope=interface+1};
                memcpy(src.ip,frame+22,16);
                uint32_t handle=XIP_ADDR_UDP+(uint32_t)udp_reply_fd(s->fd);
                xip_port_rx_commit_udp(handle,XIP_PORT_MC,&buf,n,&src);
                consumed=true;
            } else xip_port_rx_abort(&buf);
        } else consumed=true;
    }
    mc_release(s);
    return consumed;
}
/* The fd discovery replies go out on: the QUIC engine socket (the device's
 * fixed UDP port) when it is up, so a peer that discovered us can dial its
 * QUIC association straight to the source address of the reply; the mc socket
 * itself only while the engine is down. */
static int udp_reply_fd(int fallback)
{
    if(mutex && state)return state->fd;
    return fallback;
}
int h_udp_send(void *u,uint32_t handle,const uint8_t *ip,uint16_t port,uint32_t scope,const uint8_t *data,size_t len)
{
    (void)u;
    /* A datagram send is module-owned: only the runtime UDP socket handles match. The
     * handle is `ADDR_UDP + fd`; a closed socket never matches. Send and stop both run
     * on the worker, so the fd cannot be closed under this read. */
    if(!mc_state || !ip || (data==NULL && len!=0))return XIP_ERR_ARG;
    if(handle<XIP_ADDR_UDP)return XIP_ERR_NOTFOUND;
    int fd=(int)(handle-XIP_ADDR_UDP);
    /* Both runtime sockets serve module replies: the engine socket (the reply's
     * source is then the QUIC endpoint) and the mc socket. */
    if(fd!=mc_state->fd && (!mutex || !state || fd!=state->fd))return XIP_ERR_NOTFOUND;
    if(len>XIP_PKT_MAX)return XIP_ERR_SIZE;
    struct sockaddr_in6 to={.sin6_family=AF_INET6,.sin6_port=htons(port),.sin6_scope_id=scope};
    memcpy(&to.sin6_addr,ip,16);
    int n=sendto(fd,data,len,MSG_DONTWAIT,(struct sockaddr *)&to,sizeof(to));
    return n==(int)len?XIP_OK:n<0 && (errno==EAGAIN || errno==EWOULDBLOCK)?XIP_ERR_BUSY:XIP_ERR_IO;
}
void xip_mc_ensure(void)
{
    if(!xip_connected())return;
    if(!mc_mutex)mc_mutex=xSemaphoreCreateMutex();
    if(!mc_mutex)return;
    xSemaphoreTake(mc_mutex,portMAX_DELAY);if(mc_state){xSemaphoreGive(mc_mutex);return;}
    lwip_socket_thread_init();
    int fd=socket(AF_INET6,SOCK_DGRAM,0);if(fd<0){xSemaphoreGive(mc_mutex);return;}
    int off=0;(void)setsockopt(fd,IPPROTO_IPV6,IPV6_V6ONLY,&off,sizeof(off));
    struct sockaddr_in6 any={.sin6_family=AF_INET6,.sin6_port=htons(XIP_MC_UDP_PORT),.sin6_addr=IN6ADDR_ANY_INIT};
    if(fcntl(fd,F_SETFL,O_NONBLOCK)<0 || bind(fd,(struct sockaddr *)&any,sizeof(any))!=0){close(fd);xSemaphoreGive(mc_mutex);return;}
    /* The multicast membership is the important part: through esp_netif, in the lwIP
     * task, on the connected netif exactly - not a raw socket MLD set. */
    /* STA association precedes DHCP/default-route selection. IDF's join API
     * dereferences its netif argument; use an up Wi-Fi interface until a
     * default exists, and never pass NULL during a transition. */
    esp_netif_t *netif=esp_netif_get_default_netif();
    if(!netif || !esp_netif_is_netif_up(netif))netif=esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    if(!netif || !esp_netif_is_netif_up(netif))netif=esp_netif_get_handle_from_ifkey("WIFI_AP_DEF");
    if(!netif || !esp_netif_is_netif_up(netif)){close(fd);xSemaphoreGive(mc_mutex);return;}
    esp_ip6_addr_t group={0};memcpy(group.addr,XIP_MC_GROUP6,16);
    if(esp_netif_join_ip6_multicast_group(netif,&group)!=ESP_OK){close(fd);xSemaphoreGive(mc_mutex);return;}
    struct mc_state *s=calloc(1,sizeof(*s));if(!s){close(fd);xSemaphoreGive(mc_mutex);return;}
    s->fd=fd;atomic_init(&s->refs,1);
    s->watch=xip_io_watch(fd,POLLIN,mc_ready,NULL);
    if(!s->watch){free(s);close(fd);xSemaphoreGive(mc_mutex);return;}
    mc_state=s;
    xSemaphoreGive(mc_mutex);
}
void xip_mc_stop(void)
{
    if(!mc_mutex)return;
    xSemaphoreTake(mc_mutex,portMAX_DELAY);struct mc_state *s=mc_state;mc_state=NULL;xSemaphoreGive(mc_mutex);
    if(!s)return;
    xip_io_clear(s->watch);
    while(atomic_load(&s->refs)!=1)vTaskDelay(1);
    close(s->fd);free(s);
}
unsigned xip_mc_sockets(void)
{
    if(!mc_mutex)return 0;
    xSemaphoreTake(mc_mutex,portMAX_DELAY);unsigned n=mc_state?1:0;xSemaphoreGive(mc_mutex);return n;
}
