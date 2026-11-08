/* Boundary fakes for the actual C socket owner used by both firmware hosts. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include "xip.h"
#include "xip_pool.h"
#include "esp_err.h"
#define ESP_ERR_INVALID_STATE 0x103
#define ESP_LOGW(...) ((void)0)
#include "xip_lwip_io.h"
#include "freertos/FreeRTOS.h"
#define portMAX_DELAY 0
#define pdTRUE 1
#define pdPASS 1
#define pdMS_TO_TICKS(n) (n)
#define XIP_EV_IO 1
#define xSemaphoreTake(lock, ticks) ((void)(lock), (void)(ticks))
#define xSemaphoreGive(lock) ((void)(lock))
typedef void *SemaphoreHandle_t;
typedef struct { int kind; uint32_t inst; int component,method,value; uint32_t id; } xip_event_t;
typedef struct xip_instance { uint32_t id; int held,stream,component;const char *name; } test_instance;
static struct { bool wifi_reconcile; } hooks;
#define I(u) ((test_instance *)(u))
static void *worker_queue;
static bool netif_up=true;
static bool xip_connected(void){return netif_up;}
static uint64_t io_time;
static unsigned thread_inits, thread_cleanups;
static int polled_fd=-1, polled_events, poll_fault;
static bool use_real_poll, fail_task, fail_bind;
static void (*during_poll)(void);
static void *esp_netif_next_unsafe(void *previous) { (void)previous; return netif_up ? (void *)1 : NULL; }
static void lwip_socket_thread_init(void) { thread_inits++; }
static void lwip_socket_thread_cleanup(void) { thread_cleanups++; }
static int64_t test_io_time(void) { return (int64_t)io_time; }
static void vSemaphoreDelete(void *lock) { (void)lock; }
static unsigned tasks, closes, drops, queued;
static int next_fd, accepted_fd=-1, send_result, queue_full;
static xip_event_t event;
static uint8_t packet[XIP_PKT_SLOT_MIN];
static void *xSemaphoreCreateMutex(void) { return (void *)1; }
static int xTaskCreate(void (*task)(void *), const char *name, unsigned stack,
                       void *arg, unsigned priority, void *out) {
    (void)task; (void)name; (void)stack; (void)arg; (void)priority; (void)out;
    if(fail_task)return 0;
    tasks++; return pdPASS;
}
static void vTaskDelete(void *task) { (void)task; }
static void vTaskDelay(unsigned ticks) { (void)ticks; }
static int xQueueSend(void *queue, const xip_event_t *e, unsigned ticks) {
    (void)queue; (void)ticks;
    if (queue_full) return 0;
    event=*e; queued++; return pdTRUE;
}
static void drop_slot(int slot) { assert(slot==7); drops++; }
static int h_pkt_span(void *user, uint32_t pkt, uint8_t **data, size_t *cap) {
    (void)user; assert(pkt==9); *data=packet; *cap=sizeof(packet); return 0;
}
static xip_packet_ref packet_take(uint32_t pkt){assert(pkt==9);return (xip_packet_ref){.slot=7};}
static esp_err_t esp_netif_init(void){return ESP_OK;}
static int lwip_socket(int family, int type, int protocol) {
    (void)family; (void)type; (void)protocol; return next_fd++;
}
static int lwip_bind(int fd, const struct sockaddr *address, socklen_t len) {
    (void)fd; (void)address; (void)len; return fail_bind ? -1 : 0;
}
static int lwip_listen(int fd,int backlog) { (void)fd; (void)backlog; return 0; }
static int lwip_fcntl(int fd,int command,int flags) { (void)fd; (void)command; (void)flags; return 0; }
static int lwip_close(int fd) { (void)fd; closes++; return 0; }
static int lwip_accept(int fd,struct sockaddr *address,socklen_t *len) {
    (void)fd; (void)address; (void)len; int n=accepted_fd; accepted_fd=-1; return n;
}
static int lwip_send(int fd,const void *data,size_t len,int flags) {
    (void)fd; (void)flags;
    assert(data>= (void *)packet && data< (void *)(packet+sizeof(packet)));
    if(send_result<0){errno=EAGAIN;return -1;}
    return len < (size_t)send_result ? (int)len : send_result;
}
static int lwip_recv(int fd,void *data,size_t len,int flags) {
    (void)fd; (void)data; (void)len; (void)flags; errno=EAGAIN;return -1;
}
static int lwip_getpeername(int fd,struct sockaddr *address,socklen_t *len) {
    (void)fd; memset(address,0,*len); return 0;
}
static int lwip_poll(struct pollfd *fds,unsigned count,int timeout) {
    if(use_real_poll)return poll(fds,count,timeout);
    io_time+=(uint64_t)timeout*1000;
    if(during_poll){void (*fn)(void)=during_poll;during_poll=NULL;fn();}
    int n=0;
    for(unsigned i=0;i<count;i++) if(fds[i].fd==polled_fd){fds[i].revents=polled_events;n++;}
    return poll_fault ? -1 : n;
}
#define esp_timer_get_time test_io_time
#include "lwip_io_test.inc"
#undef esp_timer_get_time
#include "socket_hal_test.inc"
void test_socket_poll(void) {
    test_instance owner={.id=1,.held=1}, other={.id=2};
    uint32_t listener;
    assert(h_tcp_accept(&owner,5227,XIP_SOCKET_MODE_DIRECT,20,7,&listener)==0);
    assert(tasks==1 && sockets && state);
    int listener_fd=sockets->fd;
    accepted_fd=42;
    queue_full=1;
    assert(socket_ready(listener_fd,POLLIN,listener)==XIP_ERR_BUSY);
    sock_node *peer=sockets;
    assert(peer->fd==42 && peer->opening && !peer->pending);
    assert(find((uintptr_t)peer->watch)->events==(POLLIN|POLLOUT));
    queue_full=0;
    assert(socket_ready(42,POLLOUT,peer->handle)==0);
    assert(event.value==XIP_SOCKET_EVENT_OPEN && peer->pending && !peer->opening);
    socket_delivered(peer->handle,event.value);
    assert(h_socket_read(&owner,peer->handle,packet,1)==XIP_ERR_AGAIN);
    assert(find((uintptr_t)peer->watch)->events==POLLIN);
    assert(h_socket_close(&other,peer->handle)==XIP_ERR_ARG);
    unsigned before=queued;
    assert(socket_ready(42,POLLIN,peer->handle+1)==0 && queued==before);
    assert(h_socket_write_pkt(&owner,peer->handle,9,XIP_PKT_HEADROOM,XIP_PKT_MAX)==0);
    assert(peer->tx_slot==7 && peer->tx==packet+XIP_PKT_HEADROOM);
    assert(h_socket_write_pkt(&owner,peer->handle,9,0,1)==XIP_ERR_BUSY);
    send_result=-1;
    assert(socket_ready(42,POLLOUT,peer->handle)==0 && drops==0 && peer->tx_at==0);
    send_result=500;
    assert(socket_ready(42,POLLOUT,peer->handle)==0 && drops==0 && peer->tx_at==500);
    send_result=XIP_PKT_MAX;
    queue_full=1;
    assert(socket_ready(42,POLLOUT,peer->handle)==XIP_ERR_BUSY && drops==1 && peer->tx_slot==-2);
    queue_full=0;
    assert(socket_ready(42,POLLOUT,peer->handle)==0);
    assert(event.value==XIP_SOCKET_EVENT_WRITE_DONE && peer->tx_slot==-1 && drops==1);
    socket_delivered(peer->handle,event.value);
    assert(socket_ready(42,POLLHUP,peer->handle)==0 && event.value==XIP_SOCKET_EVENT_CLOSED);
    uint32_t old=peer->handle;
    assert(h_socket_close(&owner,old)==0);
    accepted_fd=42;
    assert(socket_ready(listener_fd,POLLIN,listener)==0);
    before=queued;
    assert(sockets->fd==42 && sockets->handle!=old);
    assert(socket_ready(42,POLLIN,old)==0 && queued==before);
    socket_release(owner.id);
    assert(!sockets && closes==3 && drops==1);
    // The existing task retires after the final socket is released.
    poll_task(NULL);
    assert(!state);
    assert(h_tcp_accept(&owner,5227,XIP_SOCKET_MODE_DIRECT,20,7,&listener)==0 && tasks==2);
    socket_release(owner.id); poll_task(NULL);
    assert(!sockets && !state);
}

static unsigned ready_calls, accepted, last_event;
static bool clear_on_ready;
static xip_io_registration latest;
static void native_ready(xip_io_registration reg,int fd,int16_t events,void *ctx) {
    assert(reg==latest && fd==80 && ctx==(void *)5);
    ready_calls++; last_event=(unsigned)events;
    if(clear_on_ready)xip_io_clear(reg);
}
static void native_accept(int fd,void *ctx) {
    assert(fd==81 && ctx==(void *)6); accepted++;
    // Callback now owns the accepted descriptor, including this rejection.
    lwip_close(fd);
}
static void replace_registration(void) {
    xip_io_registration old=latest;
    xip_io_clear(old);
    latest=xip_io_watch(80,POLLIN,native_ready,(void *)5);
    assert(latest && latest!=old);
    xip_io_suspend(old); xip_io_update(old,-1,0,0); xip_io_clear(old);
    assert(find(latest) && !find(latest)->suspended && find(latest)->fd==80);
}
void test_native_io_poll(void) {
    bool initialized=false;
    unsigned initial_tasks=tasks, initial_closes=closes, initial_threads=thread_inits;
    assert(!state);
    fail_task=true;
    assert(!xip_io_watch(80,POLLIN,native_ready,(void *)5) && !state);
    fail_task=false;
    assert(!xip_io_watch(-1,POLLIN,native_ready,(void *)5));
    assert(!xip_io_listen_tcp(0,2,native_accept,(void *)6));
    netif_up=false;
    xip_io_registration listener=xip_io_listen_tcp(5227,2,native_accept,(void *)6);
    latest=xip_io_watch(80,POLLIN,native_ready,(void *)5);
    assert(listener && latest && tasks==initial_tasks+1);
    assert(poll_once(&initialized) && !initialized && find(listener)->fd==-1);
    netif_up=true;
    polled_fd=80; polled_events=POLLIN;
    assert(poll_once(&initialized) && initialized && ready_calls==1);
    assert(thread_inits==initial_threads+1 && find(listener)->fd>=0);
    assert(last_event==POLLIN);
    xip_io_suspend(latest);
    assert(poll_once(&initialized) && ready_calls==1);
    // TCP buffered-input work needs timer callbacks even with events==0.
    polled_events=0;
    xip_io_update(latest,80,0,10);
    assert(poll_once(&initialized) && ready_calls==2 && last_event==0);
    poll_fault=1;
    assert(poll_once(&initialized) && ready_calls==2);
    poll_fault=0;
    xip_io_update(latest,80,POLLIN,0);
    polled_events=POLLIN; during_poll=replace_registration;
    assert(poll_once(&initialized) && ready_calls==2);
    assert(poll_once(&initialized) && ready_calls==3);
    accepted_fd=81; polled_fd=find(listener)->fd;
    assert(poll_once(&initialized) && accepted==1 && closes==initial_closes+1);
    // Native registrations and module sockets share a single admission budget.
    xip_io_registration extra[IO_SLOTS-2];
    for(unsigned i=0;i<IO_SLOTS-2;i++) {
        extra[i]=xip_io_watch(90+(int)i,POLLIN,native_ready,(void *)5); assert(extra[i]);
    }
    assert(!xip_io_watch(99,POLLIN,native_ready,(void *)5));
    test_instance owner={.id=3}; uint32_t handle;
    assert(h_tcp_accept(&owner,5228,XIP_SOCKET_MODE_DIRECT,20,7,&handle)==XIP_ERR_NOMEM);
    assert(!sockets && tasks==initial_tasks+1);
    for(unsigned i=0;i<IO_SLOTS-2;i++) xip_io_clear(extra[i]);
    xip_io_clear(latest);
    unsigned before=closes;
    xip_io_clear(listener);
    assert(closes==before+1); // only the service-owned listener is closed
    assert(!poll_once(&initialized) && !state);
    latest=xip_io_watch(80,POLLIN,native_ready,(void *)5);
    assert(latest && tasks==initial_tasks+2);
    polled_fd=80;polled_events=POLLIN;clear_on_ready=true;
    unsigned cleanup_before=thread_cleanups;
    poll_task(NULL);
    assert(!state && thread_cleanups==cleanup_before+1);
    clear_on_ready=false;
}

static unsigned udp_received;
static void udp_ready(xip_io_registration reg,int fd,int16_t events,void *ctx) {
    (void)ctx;
    assert(events&POLLIN);
    uint8_t received[XIP_PKT_MAX];
    ssize_t n=recv(fd,received,sizeof(received),MSG_DONTWAIT);
    assert(n==XIP_PKT_MAX);
    for(unsigned i=0;i<sizeof(received);i++) assert(received[i]==(uint8_t)(i*73));
    udp_received++;
    xip_io_suspend(reg);
}
void test_real_udp_poll(void) {
    int receiver=socket(AF_INET6,SOCK_DGRAM,0), sender=socket(AF_INET6,SOCK_DGRAM,0);
    assert(receiver>=0 && sender>=0 && !state);
    struct sockaddr_in6 address={.sin6_family=AF_INET6,.sin6_addr=IN6ADDR_LOOPBACK_INIT};
    assert(bind(receiver,(struct sockaddr *)&address,sizeof(address))==0);
    socklen_t size=sizeof(address);
    assert(getsockname(receiver,(struct sockaddr *)&address,&size)==0);
    uint8_t data[XIP_PKT_MAX];
    for(unsigned i=0;i<sizeof(data);i++)data[i]=(uint8_t)(i*73);
    xip_io_registration reg=xip_io_watch(receiver,POLLIN,udp_ready,NULL);
    assert(reg);
    use_real_poll=true;
    bool initialized=false;
    assert(sendto(sender,data,sizeof(data),0,(struct sockaddr *)&address,size)==sizeof(data));
    assert(poll_once(&initialized) && udp_received==1);
    assert(sendto(sender,data,sizeof(data),0,(struct sockaddr *)&address,size)==sizeof(data));
    assert(poll_once(&initialized) && udp_received==1); // suspended
    xip_io_update(reg,receiver,POLLIN,0);
    assert(poll_once(&initialized) && udp_received==2);
    xip_io_clear(reg);
    assert(!poll_once(&initialized) && !state);
    // Removing an external watch must not close its caller-owned fd.
    assert(fcntl(receiver,F_GETFD)>=0);
    close(receiver);close(sender);use_real_poll=false;
}

void test_mixed_module_and_native_poll(void) {
    assert(!state && !sockets);
    unsigned initial_tasks=tasks, initial_ready=ready_calls;
    latest=xip_io_watch(80,POLLIN,native_ready,(void *)5);
    test_instance owner={.id=4};uint32_t listener;
    assert(h_tcp_accept(&owner,5228,XIP_SOCKET_MODE_DIRECT,2,0,&listener)==0);
    assert(tasks==initial_tasks+1); // one task for both owners
    int listener_fd=sockets->fd;
    bool initialized=false;
    polled_fd=80;polled_events=POLLIN;
    assert(poll_once(&initialized) && ready_calls==initial_ready+1);
    polled_fd=listener_fd;accepted_fd=82;
    assert(poll_once(&initialized));
    sock_node *peer=sockets;
    assert(peer->fd==82 && peer->pending && event.value==XIP_SOCKET_EVENT_OPEN);
    socket_delivered(peer->handle,event.value);
    assert(h_socket_read(&owner,peer->handle,packet,1)==XIP_ERR_AGAIN);
    polled_fd=82;
    assert(poll_once(&initialized) && event.value==XIP_SOCKET_EVENT_READABLE);
    socket_release(owner.id);
    assert(!sockets && state && poll_once(&initialized)); // native watch survives
    xip_io_clear(latest);
    assert(!poll_once(&initialized) && !state);
}

void test_listener_retry(void) {
    assert(!state);
    bool initialized=false;
    xip_io_registration reg=xip_io_listen_tcp(5227,2,native_accept,(void *)6);
    assert(reg);
    fail_bind=true;unsigned initial_closes=closes;
    polled_fd=-1;polled_events=0;
    assert(poll_once(&initialized) && find(reg)->fd==-1 && closes==initial_closes+1);
    fail_bind=false;
    assert(poll_once(&initialized) && find(reg)->fd>=0);
    polled_fd=find(reg)->fd;polled_events=POLLERR;
    assert(poll_once(&initialized) && find(reg)->fd==-1 && closes==initial_closes+2);
    polled_fd=-1;polled_events=0;
    assert(poll_once(&initialized) && find(reg)->fd>=0);
    xip_io_clear(reg);
    assert(!poll_once(&initialized) && !state && closes==initial_closes+3);
}
