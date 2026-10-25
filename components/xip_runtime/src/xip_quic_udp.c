/* Native default UDP bearer: exact reversible peers, shared pool and polling.
 * RX commit runs outside the socket lock: it may synchronously relay to UDP. */
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include "sdkconfig.h"
#include "esp_netif.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "lwip/sockets.h"
#include "xip_runtime.h"
#include "xip_lwip_io.h"
#include "xip_udp_peers.h"
#include "internal.h"
#if CONFIG_XIP_QUIC_UDP
struct udp_state {int fd,link;xip_io_registration watch;_Atomic unsigned refs; xip_udp_peers peers;};
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
static int udp_tx(void *ctx,uint64_t dest,const xip_link_packet *pkt)
{
    struct udp_state *s=udp_acquire((uint32_t)(uintptr_t)ctx,0,0);
    if(!s)return XIP_ERR_NOTFOUND;
    xip_udp_address a;
    xSemaphoreTake(mutex,portMAX_DELAY);
    bool found=state==s && xip_udp_peer_get(&s->peers,dest,(uint64_t)esp_timer_get_time(),&a);
    xSemaphoreGive(mutex);
    if(!found){udp_release(s);return XIP_ERR_NOTFOUND;}
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
    close(s->fd);xip_link_native_unregister(s->link);free(s);
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
    s->fd=fd;s->peers.epoch=++epoch_sequence;if(!s->peers.epoch)s->peers.epoch=++epoch_sequence;atomic_init(&s->refs,1);
    s->link=xip_link_native_register("udp",XIP_PKT_MAX,0,udp_tx,(void *)(uintptr_t)s->peers.epoch);
    if(s->link<0){free(s);close(fd);xSemaphoreGive(mutex);return;}
    state=s;s->watch=xip_io_watch(fd,POLLIN,udp_ready,NULL);
    if(!s->watch){state=NULL;xip_link_native_unregister(s->link);close(fd);free(s);}
    else xip_link_native_set_state(s->link,XIP_LINK_UP,0);
    xSemaphoreGive(mutex);
}
#else
void xip_quic_udp_ensure(void){}
void xip_quic_udp_stop(void){}
unsigned xip_quic_udp_sockets(void){return 0;}
#endif
