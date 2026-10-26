#include <assert.h>
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include <poll.h>
#define portMAX_DELAY 0
static void *sockets_lock=(void *)1;
typedef struct sock_node{struct sock_node *next;uint32_t handle;bool pending;int tx_slot,fd;void *watch;}sock_node;
static sock_node one={.handle=7,.pending=true,.tx_slot=0,.fd=8,.watch=(void *)2};
static sock_node *sockets=&one;
static int16_t armed;static unsigned updates;
static void xSemaphoreTake(void *m,int delay){(void)delay;assert(m==sockets_lock);}
static void xSemaphoreGive(void *m){assert(m==sockets_lock);}
static void c_poll_update(void *watch,int fd,int16_t events){assert(watch==one.watch && fd==one.fd);armed=events;updates++;}
#include "xip.h"
#include "socket_delivered.inc"
int main(void){
    socket_delivered(7,XIP_SOCKET_EVENT_READABLE);
    assert(!one.pending && updates==1 && armed==POLLOUT);
    one.tx_slot=-1;updates=0;
    socket_delivered(7,XIP_SOCKET_EVENT_READABLE);assert(updates==0);
    socket_delivered(7,XIP_SOCKET_EVENT_WRITE_DONE);assert(updates==1 && armed==POLLIN);
    one.pending=true;socket_delivered(99,XIP_SOCKET_EVENT_OPEN);assert(one.pending && updates==1);
    return 0;
}
