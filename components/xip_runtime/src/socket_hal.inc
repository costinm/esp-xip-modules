/* Module socket owner: readiness uses the C poll task shared with native
 * UDP/TCP adapters. Module handlers run on the existing host worker. */
#include "xip_lwip_io.h"
typedef struct sock_node {
    struct sock_node *next; uint32_t handle,owner; int fd; bool listener,pending,opening;
    int32_t component,method; void *watch;
    int tx_slot; uint8_t *tx; size_t tx_at,tx_len;
} sock_node;
static sock_node *sockets;
static SemaphoreHandle_t sockets_lock;
static uint32_t socket_sequence=1;
static int socket_ready(int fd,int16_t events,uint32_t expected);
static void socket_poll_ready(xip_io_registration reg,int fd,int16_t events,void *ctx)
{
    xip_io_suspend(reg);
    if(socket_ready(fd,events,(uint32_t)(uintptr_t)ctx)==XIP_ERR_BUSY)
        xip_io_update(reg,fd,events,10);
}
static void c_poll_update(void *watch,int fd,int16_t events)
{ xip_io_update((xip_io_registration)(uintptr_t)watch,fd,events,0); }
static void c_poll_clear(void *watch)
{ xip_io_clear((xip_io_registration)(uintptr_t)watch); }

static sock_node *socket_find(uint32_t owner,uint32_t handle)
{ for(sock_node *s=sockets;s;s=s->next) if(s->handle==handle && s->owner==owner) return s; return NULL; }
static bool socket_event(sock_node *s,int value)
{
    xip_event_t ev={.kind=XIP_EV_IO,.inst=s->owner,.component=s->component,.method=s->method,.value=value,.id=s->handle};
    if(xQueueSend(worker_queue,&ev,0)!=pdTRUE) return false;
    s->pending=true; return true;
}
static sock_node *socket_add(int fd,uint32_t owner,bool listener,int32_t component,int32_t method)
{
    sock_node *s=calloc(1,sizeof(*s)); if(!s) return NULL;
    *s=(sock_node){.handle=socket_sequence++,.owner=owner,.fd=fd,.listener=listener,.opening=!listener,.component=component,.method=method,.tx_slot=-1};
    s->watch=(void *)(uintptr_t)xip_io_watch(fd,POLLIN,socket_poll_ready,(void *)(uintptr_t)s->handle); if(!s->watch){free(s);return NULL;}
    s->next=sockets;sockets=s;return s;
}
/* Called by the poll task; registry state is only used while holding this lock. */
static int socket_ready(int fd,int16_t events,uint32_t expected)
{
    if(!sockets_lock) return XIP_ERR_NOTFOUND;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=sockets;while(s && s->fd!=fd)s=s->next;
    int rc=0;
    if(s && expected && s->handle!=expected)s=NULL;
    if(s && s->listener){
        int peer=lwip_accept(fd,NULL,NULL);
        if(peer>=0){
            lwip_fcntl(peer,F_SETFL,O_NONBLOCK);
            sock_node *p=socket_add(peer,s->owner,false,s->component,s->method);
            if(!p)lwip_close(peer);else if(!socket_event(p,XIP_SOCKET_EVENT_OPEN)){rc=XIP_ERR_BUSY;c_poll_update(p->watch,peer,POLLIN|POLLOUT);}else p->opening=false;
        }
        c_poll_update(s->watch,fd,POLLIN);
    } else if(s && !s->pending){
        if(s->opening){if(socket_event(s,XIP_SOCKET_EVENT_OPEN))s->opening=false;else rc=XIP_ERR_BUSY;}
        else if(s->tx_slot==-2){if(socket_event(s,XIP_SOCKET_EVENT_WRITE_DONE))s->tx_slot=-1;else rc=XIP_ERR_BUSY;}
        else if(s->tx_slot>=0 && (events&POLLOUT)){
            int n=lwip_send(fd,s->tx+s->tx_at,s->tx_len-s->tx_at,MSG_DONTWAIT);
            if(n>0)s->tx_at+=(size_t)n;
            if(s->tx_at==s->tx_len){drop_slot(s->tx_slot);s->tx_slot=-2; if(socket_event(s,XIP_SOCKET_EVENT_WRITE_DONE))s->tx_slot=-1;else rc=XIP_ERR_BUSY;}
            else if(n<0 && errno!=EAGAIN && errno!=EWOULDBLOCK){drop_slot(s->tx_slot);s->tx_slot=-1;if(!socket_event(s,XIP_SOCKET_EVENT_CLOSED))rc=XIP_ERR_BUSY;}
            else c_poll_update(s->watch,fd,POLLIN|POLLOUT);
        } else if(!socket_event(s,(events&POLLIN)?XIP_SOCKET_EVENT_READABLE:
                (events&(POLLERR|POLLHUP|POLLNVAL))?XIP_SOCKET_EVENT_CLOSED:
                XIP_SOCKET_EVENT_WRITABLE))rc=XIP_ERR_BUSY;
    }
    xSemaphoreGive(sockets_lock);return rc;
}
static void socket_delivered(uint32_t handle)
{
    if(!sockets_lock)return;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    for(sock_node *s=sockets;s;s=s->next)if(s->handle==handle)s->pending=false;
    xSemaphoreGive(sockets_lock);
}
static void socket_destroy(sock_node **link)
{
    sock_node *s=*link;*link=s->next;c_poll_clear(s->watch);lwip_close(s->fd);
    if(s->tx_slot>=0)drop_slot(s->tx_slot);
    free(s);
}
static void socket_release(uint32_t owner)
{
    if(!sockets_lock)return;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node **link=&sockets;while(*link){if((*link)->owner==owner)socket_destroy(link);else link=&(*link)->next;}
    xSemaphoreGive(sockets_lock);
}
static int h_tcp_accept(void *u,uint16_t port,uint32_t mode,int32_t c,int32_t m,uint32_t *handle)
{
    if(!port || !handle)return XIP_ERR_ARG;
    if(mode!=XIP_SOCKET_MODE_DIRECT)return XIP_ERR_UNSUPPORTED;
    if(!sockets_lock)sockets_lock=xSemaphoreCreateMutex();
    if(!sockets_lock)return XIP_ERR_NOMEM;
    esp_netif_init();int fd=lwip_socket(AF_INET6,SOCK_STREAM,0);if(fd<0)return XIP_ERR_IO;
    struct sockaddr_in6 addr={.sin6_len=sizeof(addr),.sin6_family=AF_INET6,.sin6_port=htons(port)};
    if(lwip_fcntl(fd,F_SETFL,O_NONBLOCK)<0 || lwip_bind(fd,(struct sockaddr *)&addr,sizeof(addr))<0 || lwip_listen(fd,2)<0){lwip_close(fd);return XIP_ERR_IO;}
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_add(fd,I(u)->id,true,c,m);
    if(s)*handle=s->handle;
    xSemaphoreGive(sockets_lock);if(!s){lwip_close(fd);return XIP_ERR_NOMEM;}return 0;
}
static int h_socket_read(void *u,uint32_t handle,uint8_t *out,size_t len)
{
    if(!sockets_lock || (!out && len))return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_find(I(u)->id,handle);int rc=XIP_ERR_ARG;
    if(s && !s->listener){rc=len?lwip_recv(s->fd,out,len,MSG_DONTWAIT):0;if(rc<0)rc=errno==EAGAIN||errno==EWOULDBLOCK?XIP_ERR_AGAIN:XIP_ERR_IO;c_poll_update(s->watch,s->fd,POLLIN|(s->tx_slot!=-1?POLLOUT:0));}
    xSemaphoreGive(sockets_lock);return rc;
}
static int h_socket_write(void *u,uint32_t handle,const uint8_t *data,size_t len)
{
    if(!sockets_lock || (!data && len))return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_find(I(u)->id,handle);int rc=XIP_ERR_ARG;
    if(s && !s->listener){rc=!len?0:s->tx_slot!=-1?XIP_ERR_AGAIN:lwip_send(s->fd,data,len,MSG_DONTWAIT);if(rc<0 && rc!=XIP_ERR_AGAIN)rc=errno==EAGAIN||errno==EWOULDBLOCK?XIP_ERR_AGAIN:XIP_ERR_IO;c_poll_update(s->watch,s->fd,POLLIN|(rc==XIP_ERR_AGAIN?POLLOUT:0));}
    xSemaphoreGive(sockets_lock);return rc;
}
static int h_socket_close(void *u,uint32_t handle)
{
    if(!sockets_lock)return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node **link=&sockets;while(*link && ((*link)->owner!=I(u)->id || (*link)->handle!=handle))link=&(*link)->next;
    int rc=*link?0:XIP_ERR_ARG;if(*link)socket_destroy(link);xSemaphoreGive(sockets_lock);return rc;
}
static int h_socket_peer(void *u,uint32_t handle,uint8_t *out,uint16_t *port,uint32_t *scope)
{
    if(!sockets_lock || !out || !port || !scope)return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_find(I(u)->id,handle);struct sockaddr_in6 a;socklen_t n=sizeof(a);int rc=XIP_ERR_ARG;
    if(s && !s->listener && lwip_getpeername(s->fd,(struct sockaddr *)&a,&n)==0){memcpy(out,&a.sin6_addr,16);*port=ntohs(a.sin6_port);*scope=a.sin6_scope_id;rc=0;}
    xSemaphoreGive(sockets_lock);return rc;
}
static int h_socket_write_pkt(void *u,uint32_t handle,uint32_t pkt,uint32_t start,uint32_t len)
{
    uint8_t *data;size_t cap;int rc=h_pkt_span(u,pkt,&data,&cap);if(rc)return rc;
    if(!len || start>cap || len>cap-start || !sockets_lock)return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);sock_node *s=socket_find(I(u)->id,handle);rc=XIP_ERR_ARG;
    if(s && !s->listener){if(s->tx_slot!=-1)rc=XIP_ERR_BUSY;else{xip_held_entry e=xip_held_take(&I(u)->held,pkt);s->tx_slot=e.slot;s->tx=data+start;s->tx_at=0;s->tx_len=len;c_poll_update(s->watch,s->fd,POLLIN|POLLOUT);rc=0;}}
    xSemaphoreGive(sockets_lock);return rc;
}
