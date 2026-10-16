/* Rust hosts install the shared poller. Standalone C uses the lazy fallback
 * below; module handlers always run on the existing host worker. */
static void *(*poll_watch)(int,int16_t);
static void (*poll_update)(void *,int,int16_t);
static void (*poll_clear)(void *);
void xip_socket_poll_install(void *(*watch)(int,int16_t),void (*update)(void *,int,int16_t),void (*clear)(void *))
{ poll_watch=watch; poll_update=update; poll_clear=clear; }
typedef struct sock_node {
    struct sock_node *next; uint32_t handle,owner; int fd; bool listener,pending,opening;
    int32_t component,method; void *watch;
    int16_t poll_events;
    int tx_slot; uint8_t *tx; size_t tx_at,tx_len;
} sock_node;
static sock_node *sockets;
static SemaphoreHandle_t sockets_lock;
static uint32_t socket_sequence=1;
#define XIP_SOCKET_POLL_SLOTS 12
static bool c_poll_running;
static int socket_ready(int fd,int16_t events,uint32_t expected);
static void c_poll_task(void *arg)
{
    (void)arg;
    for (;;) {
        struct pollfd fds[XIP_SOCKET_POLL_SLOTS];
        uint32_t handles[XIP_SOCKET_POLL_SLOTS];
        int count=0;
        xSemaphoreTake(sockets_lock,portMAX_DELAY);
        if (!sockets) {
            c_poll_running=false;
            xSemaphoreGive(sockets_lock);
            vTaskDelete(NULL);
            return;
        }
        for (sock_node *s=sockets;s && count<XIP_SOCKET_POLL_SLOTS;s=s->next) {
            if (!s->poll_events) continue;
            fds[count]=(struct pollfd){.fd=s->fd,.events=s->poll_events};
            handles[count++]=s->handle;
        }
        xSemaphoreGive(sockets_lock);
        if (!count) { vTaskDelay(pdMS_TO_TICKS(10)); continue; }
        int result=lwip_poll(fds,count,50);
        if (result<=0) { if(result<0)vTaskDelay(pdMS_TO_TICKS(10)); continue; }
        for (int i=0;i<count;i++) {
            if (!fds[i].revents) continue;
            if (socket_ready(fds[i].fd,fds[i].revents,handles[i])==XIP_ERR_BUSY) {
                /* Queue pressure: keep the readiness event live until the
                 * worker can receive it. Validate the generation/handle. */
                xSemaphoreTake(sockets_lock,portMAX_DELAY);
                for (sock_node *s=sockets;s;s=s->next)
                    if(s->handle==handles[i])s->poll_events=fds[i].events;
                xSemaphoreGive(sockets_lock);
                vTaskDelay(pdMS_TO_TICKS(10));
            }
        }
    }
}
/* These callbacks run under sockets_lock; no second lock or registry exists. */
static void *c_poll_watch(int fd,int16_t events)
{
    (void)events;
    int count=0;for(sock_node *s=sockets;s;s=s->next)count++;
    if(count>=XIP_SOCKET_POLL_SLOTS)return NULL;
    if(!c_poll_running) {
        c_poll_running=true;
        if(xTaskCreate(c_poll_task,"xip_poll",4096,NULL,4,NULL)!=pdPASS) {
            c_poll_running=false;return NULL;
        }
    }
    return (void *)(uintptr_t)((unsigned)fd+1u);
}
static void c_poll_update(void *watch,int fd,int16_t events)
{
    (void)watch;
    for(sock_node *s=sockets;s;s=s->next)if(s->fd==fd){s->poll_events=events;break;}
}
static void c_poll_clear(void *watch) { (void)watch; }

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
    *s=(sock_node){.handle=socket_sequence++,.owner=owner,.fd=fd,.listener=listener,.opening=!listener,.poll_events=POLLIN,.component=component,.method=method,.tx_slot=-1};
    s->watch=poll_watch(fd,POLLIN); if(!s->watch){free(s);return NULL;}
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
    if(s && poll_watch==c_poll_watch)s->poll_events=0;
    if(s && s->listener){
        int peer=lwip_accept(fd,NULL,NULL);
        if(peer>=0){
            lwip_fcntl(peer,F_SETFL,O_NONBLOCK);
            sock_node *p=socket_add(peer,s->owner,false,s->component,s->method);
            if(!p)lwip_close(peer);else if(!socket_event(p,XIP_SOCKET_EVENT_OPEN)){rc=XIP_ERR_BUSY;poll_update(p->watch,peer,POLLIN|POLLOUT);}else p->opening=false;
        }
        poll_update(s->watch,fd,POLLIN);
    } else if(s && !s->pending){
        if(s->opening){if(socket_event(s,XIP_SOCKET_EVENT_OPEN))s->opening=false;else rc=XIP_ERR_BUSY;}
        else if(s->tx_slot==-2){if(socket_event(s,XIP_SOCKET_EVENT_WRITE_DONE))s->tx_slot=-1;else rc=XIP_ERR_BUSY;}
        else if(s->tx_slot>=0 && (events&POLLOUT)){
            int n=lwip_send(fd,s->tx+s->tx_at,s->tx_len-s->tx_at,MSG_DONTWAIT);
            if(n>0)s->tx_at+=(size_t)n;
            if(s->tx_at==s->tx_len){drop_slot(s->tx_slot);s->tx_slot=-2; if(socket_event(s,XIP_SOCKET_EVENT_WRITE_DONE))s->tx_slot=-1;else rc=XIP_ERR_BUSY;}
            else if(n<0 && errno!=EAGAIN && errno!=EWOULDBLOCK){drop_slot(s->tx_slot);s->tx_slot=-1;if(!socket_event(s,XIP_SOCKET_EVENT_CLOSED))rc=XIP_ERR_BUSY;}
            else poll_update(s->watch,fd,POLLIN|POLLOUT);
        } else if(!socket_event(s,(events&POLLIN)?XIP_SOCKET_EVENT_READABLE:
                (events&(POLLERR|POLLHUP|POLLNVAL))?XIP_SOCKET_EVENT_CLOSED:
                XIP_SOCKET_EVENT_WRITABLE))rc=XIP_ERR_BUSY;
    }
    xSemaphoreGive(sockets_lock);return rc;
}
int xip_socket_ready(int fd,int16_t events) { return socket_ready(fd,events,0); }
static void socket_delivered(uint32_t handle)
{
    if(!sockets_lock)return;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    for(sock_node *s=sockets;s;s=s->next)if(s->handle==handle)s->pending=false;
    xSemaphoreGive(sockets_lock);
}
static void socket_destroy(sock_node **link)
{
    sock_node *s=*link;*link=s->next;poll_clear(s->watch);lwip_close(s->fd);
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
    if(!poll_watch)xip_socket_poll_install(c_poll_watch,c_poll_update,c_poll_clear);
    sock_node *s=socket_add(fd,I(u)->id,true,c,m);
    if(s)*handle=s->handle;
    xSemaphoreGive(sockets_lock);if(!s){lwip_close(fd);return XIP_ERR_NOMEM;}return 0;
}
static int h_socket_read(void *u,uint32_t handle,uint8_t *out,size_t len)
{
    if(!sockets_lock || (!out && len))return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_find(I(u)->id,handle);int rc=XIP_ERR_ARG;
    if(s && !s->listener){rc=len?lwip_recv(s->fd,out,len,MSG_DONTWAIT):0;if(rc<0)rc=errno==EAGAIN||errno==EWOULDBLOCK?XIP_ERR_AGAIN:XIP_ERR_IO;poll_update(s->watch,s->fd,POLLIN|(s->tx_slot!=-1?POLLOUT:0));}
    xSemaphoreGive(sockets_lock);return rc;
}
static int h_socket_write(void *u,uint32_t handle,const uint8_t *data,size_t len)
{
    if(!sockets_lock || (!data && len))return XIP_ERR_ARG;
    xSemaphoreTake(sockets_lock,portMAX_DELAY);
    sock_node *s=socket_find(I(u)->id,handle);int rc=XIP_ERR_ARG;
    if(s && !s->listener){rc=!len?0:s->tx_slot!=-1?XIP_ERR_AGAIN:lwip_send(s->fd,data,len,MSG_DONTWAIT);if(rc<0 && rc!=XIP_ERR_AGAIN)rc=errno==EAGAIN||errno==EWOULDBLOCK?XIP_ERR_AGAIN:XIP_ERR_IO;poll_update(s->watch,s->fd,POLLIN|(rc==XIP_ERR_AGAIN?POLLOUT:0));}
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
    if(s && !s->listener){if(s->tx_slot!=-1)rc=XIP_ERR_BUSY;else{xip_held_entry e=xip_held_take(&I(u)->held,pkt);s->tx_slot=e.slot;s->tx=data+start;s->tx_at=0;s->tx_len=len;poll_update(s->watch,s->fd,POLLIN|POLLOUT);rc=0;}}
    xSemaphoreGive(sockets_lock);return rc;
}
