/* Deterministic interleaving of production socket readiness, not a model of it. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <errno.h>
#include <poll.h>
#include <fcntl.h>
#include <sys/socket.h>
#include "xip.h"
#include "xip_lwip_io.h"
#define pdTRUE 1
#define portMAX_DELAY 0
#define XIP_EV_IO 9
typedef int SemaphoreHandle_t;
static void xSemaphoreTake(int lock,int timeout){(void)lock;(void)timeout;}
static void xSemaphoreGive(int lock){(void)lock;}
typedef struct{int kind;uint32_t inst;int component,method,value;uint64_t id;} xip_event_t;
static int worker_queue;static xip_event_t event;static unsigned events;
static int xQueueSend(int queue,const xip_event_t*e,int timeout){(void)queue;(void)timeout;event=*e;events++;return 1;}
static bool suspended;static int watched_fd;static unsigned watch_sequence;static int16_t interest;
xip_io_registration xip_io_watch(int fd,int16_t mask,xip_io_ready_fn callback,void*ctx){(void)callback;(void)ctx;watched_fd=fd;interest=mask;suspended=false;return ++watch_sequence;}
void xip_io_update(xip_io_registration reg,int fd,int16_t mask,int tick){(void)reg;(void)tick;assert(fd==watched_fd);interest=mask;suspended=false;}
void xip_io_suspend(xip_io_registration reg){(void)reg;suspended=true;}
void xip_io_clear(xip_io_registration reg){(void)reg;}
static int lwip_accept(int fd,void*a,void*b){(void)fd;(void)a;(void)b;return -1;}
static int lwip_fcntl(int fd,int op,int value){(void)fd;(void)op;(void)value;return 0;}
static int lwip_close(int fd){(void)fd;return 0;}
static int lwip_send(int fd,const void*p,size_t len,int flags){(void)fd;(void)p;(void)flags;return (int)len;}
static void drop_slot(int slot){(void)slot;}
#include "socket_readiness_runtime.inc"
int main(void){
 sockets_lock=1;sock_node*s=socket_add(12,42,false,44,0);assert(s);assert(socket_find(42,s->handle)==s);
 assert(socket_event(s,XIP_SOCKET_EVENT_OPEN));s->opening=false;
 assert(s->pending&&events==1);
 /* Data is already readable before the worker handles OPEN. */
 socket_poll_ready((uint32_t)(uintptr_t)s->watch,12,POLLIN,(void*)(uintptr_t)s->handle);
 assert(suspended&&events==1);
 socket_delivered(s->handle,XIP_SOCKET_EVENT_OPEN);
 assert(!suspended&&interest==POLLIN);
 socket_poll_ready((uint32_t)(uintptr_t)s->watch,12,POLLIN,(void*)(uintptr_t)s->handle);
 assert(events==2&&event.value==XIP_SOCKET_EVENT_READABLE);
 socket_delivered(s->handle,XIP_SOCKET_EVENT_READABLE);
 assert(suspended); /* Consumer reads to rearm, preserving backpressure. */
 assert(socket_event(s,XIP_SOCKET_EVENT_WRITE_DONE));
 socket_delivered(s->handle,XIP_SOCKET_EVENT_WRITE_DONE);
 assert(!suspended&&interest==POLLIN);
 c_poll_clear(s->watch);free(s);sockets=NULL;return 0;
}
