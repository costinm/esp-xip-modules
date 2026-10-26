/* Experimental point-to-point raw IP. No private packet queue or crypto policy. */
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include "esp_netif.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "lwip/netif.h"
#include "lwip/netifapi.h"
#include "lwip/tcpip.h"
#include "lwip/pbuf.h"
#include "lwip/ip4.h"
#include "lwip/ip6.h"
#include "xip_runtime.h"
#include "xip_ip_netif.h"

struct ip_state {
    struct netif netif;
    xip_ip_config config;
    uint32_t owner, epoch;
    int link;
    unsigned users;
    bool active, closing, added;
    SemaphoreHandle_t drained;
};
/* IDF's global netif callbacks interpret non-NULL netif.state as esp_netif_t.
 * This raw interface is not an esp-netif: leave state NULL and recover our
 * owner from its first embedded member, including during netif_add callbacks. */
_Static_assert(offsetof(struct ip_state, netif)==0,"netif is the IP owner prefix");
static struct ip_state *state;
static uint32_t generation;
static portMUX_TYPE lock=portMUX_INITIALIZER_UNLOCKED;

/* The descriptor lives in this uniquely owned packet's reserved prefix, not
 * module flash or another allocation. lwIP owns the original lease until free.
 * The callback reads everything before tx_done may release the containing slot. */
struct ip_pbuf {struct pbuf_custom custom;int link,slot;bool armed;};
_Static_assert(sizeof(struct ip_pbuf)<=XIP_PKT_HEADROOM,"custom pbuf fits packet prefix");
static void packet_free(struct pbuf *p)
{
    struct ip_pbuf *b=(struct ip_pbuf *)p;
    const int link=b->link,slot=b->slot;const bool armed=b->armed;
    if(armed)xip_link_native_tx_done(link,slot,0);
}
static struct ip_state *acquire(uint32_t epoch)
{
    portENTER_CRITICAL(&lock);
    struct ip_state *s=state;
    if(!s || s->closing || s->epoch!=epoch)s=NULL;
    if(s)s->users++;
    portEXIT_CRITICAL(&lock);return s;
}
static void release(struct ip_state *s)
{
    portENTER_CRITICAL(&lock);
    s->users--;bool done=s->closing && s->users==0;
    portEXIT_CRITICAL(&lock);
    if(done)xSemaphoreGive(s->drained);
}
static bool valid_packet(const uint8_t *p,size_t n)
{
    if(!p || !n)return false;
    if((p[0]>>4)==4){
        if(n<20 || (size_t)(p[0]&15)*4>n || (p[0]&15)<5)return false;
        size_t size=((size_t)p[2]<<8)|p[3];
        /* No fragment reassembly in this experiment. DF is allowed. */
        return size==n && !(p[6]&0x3f) && p[7]==0;
    }
    if((p[0]>>4)==6){
        if(n<40 || 40+((size_t)p[4]<<8)+p[5]!=n)return false;
        unsigned next=p[6];size_t at=40;
        for(unsigned i=0;i<8;i++){
            if(next==44)return false;
            if(next!=0 && next!=43 && next!=60)return true;
            if(at+2>n)return false;
            unsigned following=p[at];size_t len=((size_t)p[at+1]+1)*8;
            if(len>n-at)return false;
            at+=len;next=following;
        }
    }
    return false;
}
static int inject(void *ctx,uint64_t token,const xip_link_packet *pkt)
{
    (void)token;
    struct ip_state *s=acquire((uint32_t)(uintptr_t)ctx);
    if(!s)return XIP_ERR_NOTFOUND;
    portENTER_CRITICAL(&lock);bool active=s->active;portEXIT_CRITICAL(&lock);
    int rc=XIP_ERR_BUSY;
    if(active){
        if(pkt->len>s->netif.mtu || !valid_packet(pkt->data,pkt->len))rc=XIP_ERR_ARG;
        else if(pkt->headroom<sizeof(struct ip_pbuf))rc=XIP_ERR_SIZE;
        else {
            struct ip_pbuf *b=(struct ip_pbuf *)xip_pool_slot(pkt->pool,pkt->slot);
            memset(b,0,sizeof(*b));b->link=s->link;b->slot=pkt->slot;b->armed=true;
            b->custom.custom_free_function=packet_free;
            struct pbuf *p=pbuf_alloced_custom(PBUF_RAW,(u16_t)pkt->len,PBUF_REF,&b->custom,pkt->data,(u16_t)pkt->len);
            if(!p)rc=XIP_ERR_NOMEM;
            else {
                err_t e=tcpip_input(p,&s->netif);
                if(e==ERR_OK)rc=0; /* async: custom free returns native ownership */
                else {b->armed=false;pbuf_free(p);rc=e==ERR_MEM?XIP_ERR_BUSY:XIP_ERR_IO;}
            }
        }
    }
    release(s);return rc;
}
static err_t output(struct netif *n,struct pbuf *p)
{
    struct ip_state *s=(struct ip_state *)n;
    portENTER_CRITICAL(&lock);bool active=!s->closing && s->active;portEXIT_CRITICAL(&lock);
    if(!active || p->tot_len>n->mtu)return ERR_IF;
    xip_link_rx_buf buf;
    if(xip_link_native_rx_begin_cached(s->link,&buf)!=0)return ERR_MEM;
    if(p->tot_len>buf.capacity || pbuf_copy_partial(p,buf.data,p->tot_len,0)!=p->tot_len){xip_link_native_rx_abort(&buf);return ERR_MEM;}
    /* The stack retains this borrowed pbuf chain (including TCP retransmit
     * storage), so output must copy once into a new native owned pool lease. */
    xip_link_native_rx_commit(s->link,&buf,p->tot_len,0,0);return ERR_OK;
}
static err_t output4(struct netif *n,struct pbuf *p,const ip4_addr_t *a){(void)a;return output(n,p);}
static err_t output6(struct netif *n,struct pbuf *p,const ip6_addr_t *a){(void)a;return output(n,p);}
static err_t init(struct netif *n)
{
    struct ip_state *s=(struct ip_state *)n;n->name[0]='x';n->name[1]='i';n->mtu=s->config.mtu;
    n->output=output4;n->output_ip6=output6;
    /* Raw point-to-point IP: no Ethernet, ARP, ND framing or default route. */
    n->flags=0;return ERR_OK;
}
static void up(struct netif *n)
{
    struct ip_state *s=(struct ip_state *)n;
    ip6_addr_t addr;memset(&addr,0,sizeof(addr));memcpy(addr.addr,s->config.ipv6,16);
    if(!ip6_addr_isany(&addr)){netif_ip6_addr_set(n,0,&addr);netif_ip6_addr_set_state(n,0,IP6_ADDR_PREFERRED);}
    netif_set_ip6_autoconfig_enabled(n,0);
    netif_set_up(n);netif_set_link_up(n);
}
static void down(struct netif *n){netif_set_link_down(n);netif_set_down(n);}
static void remove_interface(struct netif *n){down(n);netif_remove(n);}
static bool any(const uint8_t *p,size_t n){for(size_t i=0;i<n;i++)if(p[i])return true;return false;}
int xip_ip_open(uint32_t owner,const xip_ip_config *c,uint32_t *link)
{
    if(!c || !link || c->size<sizeof(*c) || c->reserved || (!any(c->ipv4,4)&&!any(c->ipv6,16)))return XIP_ERR_ARG;
    uint16_t mtu=c->mtu?c->mtu:1280;if(mtu<1280 || mtu>XIP_PKT_MAX)return XIP_ERR_ARG;
    if(any(c->ipv4,4) && !any(c->netmask,4))return XIP_ERR_ARG;
    portENTER_CRITICAL(&lock);int orphan=state && state->owner==0 && !state->active?state->link:-1;portEXIT_CRITICAL(&lock);
    if(orphan>=0 && xip_ip_close(0,(uint32_t)orphan)<0)return XIP_ERR_BUSY;
    if(esp_netif_init()!=ESP_OK)return XIP_ERR_IO;
    struct ip_state *s=calloc(1,sizeof(*s));if(!s)return XIP_ERR_NOMEM;
    s->drained=xSemaphoreCreateBinary();if(!s->drained){free(s);return XIP_ERR_NOMEM;}
    s->config=*c;s->config.mtu=mtu;s->owner=owner;
    portENTER_CRITICAL(&lock);
    bool busy=state!=NULL;
    if(!busy){generation++;if(!generation)generation++;s->epoch=generation;state=s;}
    portEXIT_CRITICAL(&lock);
    if(busy){vSemaphoreDelete(s->drained);free(s);return XIP_ERR_BUSY;}
    s->link=xip_link_native_register("ip",mtu,0,inject,(void *)(uintptr_t)s->epoch);
    if(s->link<0){int rc=s->link;portENTER_CRITICAL(&lock);state=NULL;portEXIT_CRITICAL(&lock);vSemaphoreDelete(s->drained);free(s);return rc;}
    ip4_addr_t address,mask,gateway;
    memcpy(&address.addr,c->ipv4,4);memcpy(&mask.addr,c->netmask,4);memcpy(&gateway.addr,c->gateway,4);
    err_t err=netifapi_netif_add(&s->netif,&address,&mask,&gateway,NULL,init,tcpip_input);
    if(err!=ERR_OK){xip_ip_close(owner,(uint32_t)s->link);return XIP_ERR_IO;}
    s->added=true;
    portENTER_CRITICAL(&lock);s->active=true;portEXIT_CRITICAL(&lock);
    if(netifapi_netif_common(&s->netif,up,NULL)!=ERR_OK){xip_ip_close(owner,(uint32_t)s->link);return XIP_ERR_IO;}
    xip_link_native_set_state(s->link,XIP_LINK_UP,0);xip_link_native_rx_prepare();
    *link=(uint32_t)s->link;xip_host_ip_connected(true);return XIP_OK;
}
int xip_ip_set_up(uint32_t owner,uint32_t link,uint32_t active)
{
    if(active>1)return XIP_ERR_ARG;
    portENTER_CRITICAL(&lock);struct ip_state *s=state;
    bool valid=s && !s->closing && s->owner==owner && s->link==(int)link;
    bool was_active=valid && s->active;
    if(valid)s->active=active!=0;
    portEXIT_CRITICAL(&lock);if(!valid)return XIP_ERR_NOTFOUND;
    if(netifapi_netif_common(&s->netif,active?up:down,NULL)!=ERR_OK) {
        portENTER_CRITICAL(&lock);s->active=was_active;portEXIT_CRITICAL(&lock);
        return XIP_ERR_IO;
    }
    xip_link_native_set_state(s->link,active?XIP_LINK_UP:XIP_LINK_DOWN,0);
    xip_host_ip_connected(active!=0);return XIP_OK;
}
int xip_ip_close(uint32_t owner,uint32_t link)
{
    portENTER_CRITICAL(&lock);struct ip_state *s=state;
    bool valid=s && !s->closing && s->owner==owner && s->link==(int)link;
    unsigned users=0;if(valid){s->closing=true;s->active=false;users=s->users;}
    portEXIT_CRITICAL(&lock);if(!valid)return XIP_ERR_NOTFOUND;
    xip_link_native_set_state(s->link,XIP_LINK_DOWN,0);
    if(users)xSemaphoreTake(s->drained,portMAX_DELAY);
    /* Ordered after all accepted input, aborts TCP/UDP addresses and releases
     * retained custom pbufs before the interface state is freed. */
    if(s->added && netifapi_netif_common(&s->netif,remove_interface,NULL)!=ERR_OK) {
        /* Never free a netif still referenced by the TCP/IP thread. A failed
         * explicit close can retry; an owner-stop orphan is reclaimed at open. */
        portENTER_CRITICAL(&lock);s->closing=false;portEXIT_CRITICAL(&lock);
        xip_host_ip_connected(false);return XIP_ERR_BUSY;
    }
    xip_link_native_unregister(s->link);
    portENTER_CRITICAL(&lock);state=NULL;portEXIT_CRITICAL(&lock);
    xip_host_ip_connected(false);vSemaphoreDelete(s->drained);free(s);return XIP_OK;
}
void xip_ip_release(uint32_t owner)
{
    portENTER_CRITICAL(&lock);struct ip_state *s=state;int link=s&&s->owner==owner?s->link:-1;portEXIT_CRITICAL(&lock);
    if(link>=0 && xip_ip_close(owner,(uint32_t)link)<0) {
        portENTER_CRITICAL(&lock);
        if(state && state->owner==owner)state->owner=0;
        portEXIT_CRITICAL(&lock);
    }
}
void xip_ip_suspend_all(void)
{
    portENTER_CRITICAL(&lock);struct ip_state *s=state;uint32_t owner=s?s->owner:0;int link=s?s->link:-1;portEXIT_CRITICAL(&lock);
    if(link>=0 && xip_ip_close(owner,(uint32_t)link)<0) {
        portENTER_CRITICAL(&lock);
        if(state && state->owner==owner)state->owner=0;
        portEXIT_CRITICAL(&lock);
    }
}
