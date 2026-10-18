/* Real POSIX sockets exercise the production C HAL and its C poll fallback. */
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
#define pdTRUE 1
#define pdPASS 1
#define portMAX_DELAY 0
#define pdMS_TO_TICKS(n) (n)
#define XIP_EV_IO 9
#define lwip_socket socket
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
typedef struct { void (*entry)(void *);void *arg; } task_start;
static pthread_t tasks[8];static int tasks_started;static atomic_int tasks_done,fail_create;
static void *task_thread(void *arg) { task_start start=*(task_start *)arg;free(arg);start.entry(start.arg);atomic_fetch_add(&tasks_done,1);return NULL; }
static int xTaskCreate(void (*entry)(void *),const char *name,int stack,void *arg,int priority,void *handle) {
    (void)name;(void)stack;(void)priority;(void)handle;
    if(atomic_load(&fail_create))return 0;
    task_start *start=malloc(sizeof(*start));assert(start);*start=(task_start){entry,arg};
    assert(tasks_started<8);assert(!pthread_create(&tasks[tasks_started++],NULL,task_thread,start));return pdPASS;
}
static void vTaskDelay(int ms) { usleep((unsigned)ms*1000); }
static void vTaskDelete(void *task) { (void)task; }
typedef struct {uint32_t id;xip_held held;} instance;
#define I(p) ((instance *)(p))
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
static uint8_t packet[1204];
static int h_pkt_span(void *u,uint32_t pkt,uint8_t **data,size_t *cap) {
    if(!xip_held_get(&I(u)->held,pkt))return XIP_ERR_ARG;
    *data=packet;*cap=sizeof(packet);return 0;
}
static void drop_slot(int slot) { assert(slot==7);atomic_fetch_add(&dropped,1); }
#include "../../components/xip_runtime/src/socket_hal.inc"
static xip_event_t next_event(int value) {
    struct timespec deadline;clock_gettime(CLOCK_REALTIME,&deadline);deadline.tv_sec+=5;
    pthread_mutex_lock(&queue_lock);
    while(queue_at==queue_end)assert(pthread_cond_timedwait(&queue_ready,&queue_lock,&deadline)==0);
    xip_event_t event=queue[queue_at++%64];pthread_mutex_unlock(&queue_lock);
    assert(event.kind==XIP_EV_IO && event.inst==42 && event.component==44 && event.method==100);
    assert(event.value==value);socket_delivered((uint32_t)event.id);return event;
}
static void wait_counter(atomic_int *counter,int minimum) {
    for(int i=0;i<5000 && atomic_load(counter)<minimum;i++)usleep(1000);
    assert(atomic_load(counter)>=minimum);
}
static uint16_t free_port(void) {
    int fd=socket(AF_INET6,SOCK_STREAM,0);assert(fd>=0);
    struct sockaddr_in6 addr={.sin6_family=AF_INET6,.sin6_addr=IN6ADDR_LOOPBACK_INIT};
    assert(!bind(fd,(struct sockaddr *)&addr,sizeof(addr)));socklen_t size=sizeof(addr);
    assert(!getsockname(fd,(struct sockaddr *)&addr,&size));close(fd);return ntohs(addr.sin6_port);
}
int main(void) {
    instance in={.id=42},other={.id=43};xip_held_init(&in.held,4);xip_held_init(&other.held,4);
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
    uint32_t pkt=xip_held_add(&in.held,7,64,0,XIP_HELD_OWNED);memcpy(packet+64,"packet",6);
    assert(h_socket_write_pkt(&other,peer,pkt,64,6)==XIP_ERR_ARG && xip_held_get(&in.held,pkt));
    attempts=atomic_load(&queue_attempts);atomic_store(&queue_blocked,1);
    assert(h_socket_write_pkt(&in,peer,pkt,64,6)==0 && !xip_held_get(&in.held,pkt));
    assert(recv(client,bytes,sizeof(bytes),0)==6 && !memcmp(bytes,"packet",6));
    wait_counter(&queue_attempts,attempts+1);atomic_store(&queue_blocked,0);
    next_event(XIP_SOCKET_EVENT_WRITE_DONE);assert(atomic_load(&dropped)==1);
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==XIP_ERR_AGAIN);
    shutdown(client,SHUT_WR);next_event(XIP_SOCKET_EVENT_READABLE);
    assert(h_socket_read(&in,peer,bytes,sizeof(bytes))==0);
    assert(h_socket_close(&other,peer)==XIP_ERR_ARG);
    pkt=xip_held_add(&in.held,7,64,0,XIP_HELD_OWNED);
    assert(h_socket_write_pkt(&in,peer,pkt,64,6)==0);
    assert(!xip_held_get(&in.held,pkt));
    assert(h_socket_close(&in,peer)==0);
    close(client);socket_release(in.id);wait_counter(&tasks_done,1);pthread_join(tasks[0],NULL);
    assert(!sockets && !c_poll_running && atomic_load(&dropped)==2);
    assert(h_tcp_accept(&in,free_port(),XIP_SOCKET_MODE_DIRECT,44,100,&listener)==0);
    /* A stale snapshot cannot act on a reused fd with a different handle. */
    int fd=sockets->fd;assert(socket_ready(fd,POLLIN,listener+100)==0 && sockets->poll_events==POLLIN);
    socket_release(in.id);wait_counter(&tasks_done,2);pthread_join(tasks[1],NULL);
    assert(!sockets && !c_poll_running);
    pthread_mutex_destroy(sockets_lock);free(sockets_lock);sockets_lock=NULL;
    puts("socket HAL: direct I/O, queue retries, packet ownership, EOF and task shutdown ok");
}
