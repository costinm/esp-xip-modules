/* Real POSIX sockets exercise the production C HAL; a deterministic poll
 * adapter drives the current shared readiness API without ESP task stubs. */
#define _DEFAULT_SOURCE
#include <assert.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <time.h>
#include <unistd.h>
#include "xip.h"
#include "xip_pool.h"
#define ESP_OK 0
#define ESP_ERR_INVALID_STATE 1
#define ESP_LOGW(...) ((void)0)
#define TAG "test"
typedef int esp_err_t;
#define pdTRUE 1
#define pdPASS 1
#define portMAX_DELAY 0
#define pdMS_TO_TICKS(n) (n)
#define XIP_EV_IO 9
#define lwip_socket socket
#define lwip_setsockopt setsockopt
#define lwip_bind bind
#define lwip_listen listen
#define lwip_accept accept
#define lwip_close close
#define lwip_fcntl fcntl
#define lwip_recv recv
#define lwip_send send
#define lwip_getpeername getpeername
#define lwip_poll poll
/* Linux has no BSD length field; its unused bind flowinfo carries the value. */
#define sin6_len sin6_flowinfo
static int esp_netif_init(void) { return 0; }
typedef pthread_mutex_t *SemaphoreHandle_t;
static SemaphoreHandle_t xSemaphoreCreateMutex(void) {
    SemaphoreHandle_t lock=malloc(sizeof(*lock));assert(lock);
    assert(!pthread_mutex_init(lock,NULL));return lock;
}
static void xSemaphoreTake(SemaphoreHandle_t lock,int timeout) { (void)timeout;assert(!pthread_mutex_lock(lock)); }
static void xSemaphoreGive(SemaphoreHandle_t lock) { assert(!pthread_mutex_unlock(lock)); }
static void lwip_socket_thread_init(void) {}
typedef struct xip_instance {uint32_t id,stream;int component;const char *name;} instance;
#define I(p) ((instance *)(p))
static struct {bool wifi_reconcile;} hooks;
static bool connected;
static bool xip_connected(void) { return connected; }
#include "xip_lwip_io.h"
typedef struct {int fd;int16_t interest;xip_io_ready_fn callback;void *ctx;} watch_entry;
static watch_entry watches[32];
static unsigned watch_sequence;
static atomic_int fail_create;
xip_io_registration xip_io_watch(int fd,int16_t interest,xip_io_ready_fn callback,void *ctx) {
    if(atomic_load(&fail_create))return 0;
    unsigned reg=++watch_sequence;assert(reg<32);watches[reg]=(watch_entry){fd,interest,callback,ctx};return reg;
}
void xip_io_update(xip_io_registration reg,int fd,int16_t interest,int tick) {
    (void)tick;assert(reg<32 && watches[reg].callback && watches[reg].fd==fd);watches[reg].interest=interest;
}
void xip_io_suspend(xip_io_registration reg) { assert(reg<32);watches[reg].interest=0; }
void xip_io_clear(xip_io_registration reg) { assert(reg<32);watches[reg]=(watch_entry){0}; }
static void pump(void) {
    for(unsigned reg=1;reg<=watch_sequence;reg++) {
        watch_entry w=watches[reg];if(!w.callback || !w.interest)continue;
        struct pollfd p={.fd=w.fd,.events=w.interest};
        if(poll(&p,1,0)>0)w.callback(reg,w.fd,p.revents,w.ctx);
    }
}
typedef struct {int kind;uint32_t inst;int32_t component,method,value;uint64_t id;} xip_event_t;
static int worker_queue;
static xip_event_t queue[64];static unsigned queue_at,queue_end;
static pthread_mutex_t queue_lock=PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t queue_ready=PTHREAD_COND_INITIALIZER;
static atomic_int queue_blocked,queue_attempts,dropped;
static int xQueueSend(int q,const xip_event_t *event,int timeout) {
    (void)q;(void)timeout;atomic_fetch_add(&queue_attempts,1);
    if(atomic_load(&queue_blocked))return 0;
    pthread_mutex_lock(&queue_lock);assert(queue_end-queue_at<64);queue[queue_end++%64]=*event;
    pthread_cond_signal(&queue_ready);pthread_mutex_unlock(&queue_lock);return pdTRUE;
}
static uint8_t *packet;static xip_pool *pool;
static int h_pkt_span(void *u,uint32_t pkt,uint8_t **data,size_t *cap) {
    (void)u;if(!xip_packet_get(pool,pkt))return XIP_ERR_ARG;
    *data=packet;*cap=1204;return 0;
}
static void drop_slot(int slot) { assert(slot==0);xip_pool_release(pool,slot);atomic_fetch_add(&dropped,1); }
static xip_packet_ref packet_take(uint32_t h){return *xip_packet_get(pool,h);}
static uint32_t new_packet(void){int slot=xip_pool_acquire(pool,0);assert(slot==0);packet=xip_pool_slot(pool,slot);return xip_packet_handle(pool,slot,64,0);}
#include "../../components/xip_runtime/src/socket_hal.inc"
static xip_event_t next_event(int value) {
    for(int i=0;i<5000 && queue_at==queue_end;i++){pump();usleep(1000);}
    assert(queue_at!=queue_end);
    xip_event_t event=queue[queue_at++%64];
    assert(event.kind==XIP_EV_IO && event.inst==42 && event.component==44 && event.method==100);
    assert(event.value==value);socket_delivered((uint32_t)event.id,event.value);return event;
}
static void wait_counter(atomic_int *counter,int minimum) {
    for(int i=0;i<5000 && atomic_load(counter)<minimum;i++){pump();usleep(1000);}
    assert(atomic_load(counter)>=minimum);
}
static uint16_t free_port(void) {
    int fd=socket(AF_INET6,SOCK_STREAM,0);assert(fd>=0);
    struct sockaddr_in6 addr={.sin6_family=AF_INET6,.sin6_addr=IN6ADDR_LOOPBACK_INIT};
    assert(!bind(fd,(struct sockaddr *)&addr,sizeof(addr)));socklen_t size=sizeof(addr);
    assert(!getsockname(fd,(struct sockaddr *)&addr,&size));close(fd);return ntohs(addr.sin6_port);
}
int main(void) {
    instance in={.id=42},other={.id=43};pool=xip_pool_create(8,1204);assert(pool);packet=xip_pool_slot(pool,0);
    uint32_t listener;uint16_t port=free_port();
    atomic_store(&fail_create,1);assert(h_tcp_accept(&in,port,XIP_SOCKET_MODE_DIRECT,44,100,&listener)==XIP_ERR_NOMEM);
    atomic_store(&fail_create,0);assert(h_tcp_accept(&in,port,XIP_SOCKET_MODE_DIRECT,44,100,&listener)==0);
    int client=socket(AF_INET6,SOCK_STREAM,0);assert(client>=0);
    struct timeval timeout={.tv_sec=5};setsockopt(client,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));
    struct sockaddr_in6 address={.sin6_family=AF_INET6,.sin6_port=htons(port),.sin6_addr=IN6ADDR_LOOPBACK_INIT};
    atomic_store(&queue_blocked,1);assert(connect(client,(struct sockaddr *)&address,sizeof(address))==0);
    wait_counter(&queue_attempts,1);atomic_store(&queue_blocked,0);
    xip_event_t event=next_event(XIP_SOCKET_EVENT_OPEN);uint32_t peer=(uint32_t)event.id;assert(peer!=listener);
    uint8_t bytes[16],ip[16];uint16_t remote_port;uint32_t scope;
    assert(h_socket_peer(&in,peer,ip,&remote_port,&scope)==0 && remote_port);
    assert(h_socket_read(&other,peer,bytes,sizeof(bytes))==XIP_ERR_ARG);
    assert(h_socket_read(&in,peer,NULL,0)==0);
    assert(h_socket_write(&in,peer,NULL,0)==0);
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==XIP_ERR_AGAIN);
    assert(send(client,"abcdef",6,0)==6);next_event(XIP_SOCKET_EVENT_READABLE);
    assert(h_socket_read(&in,peer,bytes,3)==3 && !memcmp(bytes,"abc",3));
    next_event(XIP_SOCKET_EVENT_READABLE);
    assert(h_socket_read(&in,peer,bytes,3)==3 && !memcmp(bytes,"def",3));
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==XIP_ERR_AGAIN);
    int attempts=atomic_load(&queue_attempts);atomic_store(&queue_blocked,1);
    assert(send(client,"retry",5,0)==5);wait_counter(&queue_attempts,attempts+1);atomic_store(&queue_blocked,0);
    next_event(XIP_SOCKET_EVENT_READABLE);assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==5);
    assert(h_socket_write(&in,peer,(const uint8_t *)"reply",5)==5);
    assert(recv(client,bytes,sizeof(bytes),0)==5 && !memcmp(bytes,"reply",5));
    uint32_t pkt=new_packet();memcpy(packet+64,"packet",6);
    assert(h_socket_write_pkt(&other,peer,pkt,64,6)==XIP_ERR_ARG && xip_packet_get(pool,pkt));
    attempts=atomic_load(&queue_attempts);atomic_store(&queue_blocked,1);
    assert(h_socket_write_pkt(&in,peer,pkt,64,6)==0 && xip_packet_get(pool,pkt));
    pump();
    assert(recv(client,bytes,sizeof(bytes),0)==6 && !memcmp(bytes,"packet",6));
    wait_counter(&queue_attempts,attempts+1);atomic_store(&queue_blocked,0);
    next_event(XIP_SOCKET_EVENT_WRITE_DONE);assert(atomic_load(&dropped)==1);
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==XIP_ERR_AGAIN);
    shutdown(client,SHUT_WR);next_event(XIP_SOCKET_EVENT_READABLE);
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==0);
    /* lwIP may report EOF as HUP alone. A deferred flash response still needs
     * the write half; HUP must not become a terminal module CLOSED event. */
    sock_node *active=socket_find(in.id,peer);assert(active);
    assert(socket_ready(active->fd,POLLHUP,peer)==0);
    next_event(XIP_SOCKET_EVENT_READABLE);
    assert(socket_ready(active->fd,POLLERR,peer)==0);
    next_event(XIP_SOCKET_EVENT_READABLE);
    assert(h_socket_write(&in,peer,(const uint8_t *)"durable",7)==7);
    assert(recv(client,bytes,sizeof(bytes),0)==7 && !memcmp(bytes,"durable",7));
    assert(h_socket_close(&other,peer)==XIP_ERR_ARG);
    pkt=new_packet();
    assert(h_socket_write_pkt(&in,peer,pkt,64,6)==0);
    assert(xip_packet_get(pool,pkt)); /* queued TX remains owned until cancellation */
    assert(h_socket_close(&in,peer)==0);
    close(client);socket_release(in.id);
    assert(!sockets && atomic_load(&dropped)==2);
    assert(h_tcp_accept(&in,free_port(),XIP_SOCKET_MODE_DIRECT,44,100,&listener)==0);
    /* A stale snapshot cannot act on a reused fd with a different handle. */
    int fd=sockets->fd;assert(socket_ready(fd,POLLIN,listener+100)==0 && watches[(uint32_t)(uintptr_t)sockets->watch].interest==POLLIN);
    socket_release(in.id);
    assert(!sockets);
    /* A flash server closes first after its durable reply. Reconnect must be
     * able to rebind the same service while that transfer is in TIME_WAIT. */
    port=free_port();
    assert(h_tcp_accept(&in,port,XIP_SOCKET_MODE_DIRECT,44,100,&listener)==0);
    client=socket(AF_INET6,SOCK_STREAM,0);assert(client>=0);
    address.sin6_port=htons(port);assert(connect(client,(struct sockaddr *)&address,sizeof(address))==0);
    peer=(uint32_t)next_event(XIP_SOCKET_EVENT_OPEN).id;
    assert(h_socket_close(&in,peer)==0);assert(recv(client,bytes,sizeof(bytes),0)==0);
    close(client);socket_release(in.id);
    assert(h_tcp_accept(&in,port,XIP_SOCKET_MODE_DIRECT,44,100,&listener)==0);
    socket_release(in.id);
    /* Declarative listener defers until a physical interface is ready. */
    in.stream=3;in.component=44;in.name="test";hooks.wifi_reconcile=true;
    stream_accept(&in);assert(!sockets);
    connected=true;stream_accept(&in);assert(sockets);
    socket_network_down();assert(!sockets);
    pthread_mutex_destroy(sockets_lock);free(sockets_lock);sockets_lock=NULL;
    puts("socket HAL: direct I/O, queue retries, packet ownership, EOF and network teardown ok");
}
