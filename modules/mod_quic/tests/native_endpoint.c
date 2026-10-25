#include <assert.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <stdio.h>
#include "xip.h"
#include "xip_pool.h"
#include "xip_udp_peers.h"
#define CONFIG_XIP_PKT_INBOX 4
#define XIP_QUIC_MAX_PACKET 1448
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
#define pdTRUE 1
#define XIP_EV_PACKET 7
static int registry_lock,worker_queue;
typedef struct{uint8_t kind;uint32_t inst;}xip_event_t;
struct xip_instance{uint32_t id;int component;xip_held held;struct{uint32_t pkt_sent;}stats;
 unsigned pkt_head,pkt_count;struct{int16_t slot;uint16_t start,len;int32_t component,method,value;uint32_t from,lflags,time_us;uint64_t id;}pkt_in[4];};
static struct xip_instance instance;
static xip_pool *pool;
static _Atomic uint32_t quic_module_owner;
static _Atomic bool quic_classifier;
static unsigned udp_ensure_calls;
static void xip_quic_udp_ensure(void){udp_ensure_calls++;}
static int queue_accept=1,tx_result=-5,physical_slot=-1,physical_sender,physical_method;
static uint8_t *physical_base;
static struct xip_instance *I(void *p){return p;}
static xip_pool *pool_get(void){return pool;}
static void drop_slot(int s){xip_pool_release(pool,s);}
static bool known_locked(struct xip_instance *p){return p==&instance;}
static struct xip_instance *find(uint32_t id){return instance.id==id?&instance:NULL;}
static int xQueueSend(int q,const xip_event_t *event,int wait){(void)q;(void)event;(void)wait;return queue_accept;}
#include "runtime_deliver.inc"
typedef struct{int link;const char *name;uint64_t source;int32_t signal;uint32_t time_us,port;}xip_rx_info;
static int link_tx(uint32_t link,int slot,uint16_t start,uint16_t len,uint64_t peer,int sender,int component,int method)
{
 (void)link;(void)start;(void)len;(void)peer;assert(component==-2147483635);
 if(tx_result<0)return tx_result;
 physical_slot=slot;physical_base=xip_pool_slot(pool,slot);physical_sender=sender;physical_method=method;return 0;
}
#include "xip_quic_module.inc"
static void init(void){atomic_store(&quic_classifier,false);unsigned before=udp_ensure_calls;(void)quic_module_table();memset(&instance,0,sizeof(instance));instance.id=1;instance.component=48;xip_held_init(&instance.held,4);pool=xip_pool_create(8,1704);assert(pool);assert(c_quic_attach(&instance,100)==0);assert(udp_ensure_calls==before+1);assert(!atomic_load(&quic_classifier));assert(atomic_load(&quic_module_owner)==1);}
static void clear(void){bool native_on=atomic_load(&quic_classifier);c_quic_detach(&instance);assert(!atomic_load(&quic_module_owner));assert(atomic_load(&quic_classifier)==native_on);for(unsigned i=0;i<XIP_HELD_MAX;i++){if(instance.held.e[i].state){drop_slot(instance.held.e[i].slot);instance.held.e[i].state=0;}}assert(xip_pool_available(pool)==8);xip_pool_destroy(pool);pool=NULL;}
static void ownership(void)
{
 init();uint32_t h;assert(c_quic_acquire(&instance,2,&h)==0);xip_held_entry before=*xip_held_get(&instance.held,h);
 uint8_t *base=xip_pool_slot(pool,before.slot);memset(base+144,0x71,32);uint32_t ticket;
 for(unsigned i=0;i<2;i++){tx_result=i?-4:-5;assert(c_quic_try_send(&instance,h,144,32,65537,123,&ticket)==tx_result);assert(!memcmp(&before,xip_held_get(&instance.held,h),sizeof(before)));assert(xip_pool_slot(pool,before.slot)==base);assert(base[144]==0x71);}
 tx_result=0;assert(c_quic_try_send(&instance,h,144,32,65537,123,&ticket)==0);assert(!xip_held_get(&instance.held,h));assert(physical_base==base);
 instance.pkt_count=4; // full generic inbox must not lose completion
 assert(quic_module_complete(physical_slot,physical_sender,-2147483635,physical_method,0,12));
 xip_quic_tx_completion done={.size=sizeof(done)};assert(c_quic_poll_tx(&instance,&done)==1);assert(done.ticket==ticket);assert(done.elapsed_us==12);assert(xip_pool_slot(pool,xip_held_get(&instance.held,done.packet)->slot)==base);assert(base[144]==0x71);
 instance.pkt_count=0;atomic_store(&quic_classifier,true);clear();
}
static void epoch_and_backpressure(void)
{
 init();uint32_t h,ticket;assert(c_quic_acquire(&instance,0,&h)==0);tx_result=0;assert(c_quic_try_send(&instance,h,144,8,65537,1,&ticket)==0);
 int oldslot=physical_slot,oldsender=physical_sender,oldticket=physical_method;
 c_quic_detach(&instance);instance.id=2;assert(c_quic_attach(&instance,100)==0);
 assert(quic_module_complete(oldslot,oldsender,-2147483635,oldticket,0,1));assert(xip_pool_available(pool)==8);
 assert(c_quic_acquire(&instance,0,&h)==0);int slot=xip_held_get(&instance.held,h)->slot;
 instance.pkt_count=0;assert(deliver_checked(&instance,1,slot,144,8,48,100,65537,1,0,0,0)==XIP_ERR_NOTFOUND);assert(!instance.pkt_count);
 int raw=xip_pool_acquire(pool,0);assert(raw>=0);xip_rx_info info={.link=65537,.name="udp",.source=91,.signal=-43,.time_us=123};
 assert(quic_module_offer(&info,raw,144,8));assert(instance.pkt_count==1);assert(instance.pkt_in[0].value==-43);assert(instance.pkt_in[0].time_us==123);drop_slot(raw);instance.pkt_count=0;
 queue_accept=0;assert(deliver_checked(&instance,2,slot,144,8,48,100,65537,1,0,0,0)==XIP_ERR_BUSY);assert(!instance.pkt_count);queue_accept=1;
 clear();
}
static void completion_capacity(void)
{
 init();tx_result=0;int slots[4],senders[4],tickets[4];uint32_t h,t;
 for(unsigned i=0;i<4;i++){assert(c_quic_acquire(&instance,0,&h)==0);assert(c_quic_try_send(&instance,h,144,8,65537,1,&t)==0);slots[i]=physical_slot;senders[i]=physical_sender;tickets[i]=physical_method;}
 assert(c_quic_acquire(&instance,0,&h)==0);xip_held_entry held=*xip_held_get(&instance.held,h);assert(c_quic_try_send(&instance,h,144,8,65537,1,&t)==XIP_ERR_BUSY);assert(xip_held_get(&instance.held,h)->slot==held.slot);
 for(unsigned i=0;i<4;i++)quic_module_complete(slots[i],senders[i],-2147483635,tickets[i],0,1);
 xip_quic_tx_completion out={.size=sizeof(out)};
 for(unsigned i=0;i<3;i++)assert(c_quic_poll_tx(&instance,&out)==1);
 assert(c_quic_poll_tx(&instance,&out)==XIP_ERR_BUSY);xip_held_entry e=xip_held_take(&instance.held,h);drop_slot(e.slot);assert(c_quic_poll_tx(&instance,&out)==1);clear();
}
static void peers(void)
{
 xip_udp_peers t={.epoch=7};xip_udp_address a={.ip={0x20,1},.port=3339,.scope=2},b=a,out;
 uint64_t first=xip_udp_peer_note(&t,&a,1);assert(first);assert(xip_udp_peer_get(&t,first,2,&out));assert(xip_udp_same(&a,&out));
 b.ip[0]=0x21;uint64_t second=xip_udp_peer_note(&t,&b,3);assert(first!=second);b=a;b.scope=3;assert(xip_udp_peer_note(&t,&b,4)!=first);b=a;b.port=3340;assert(xip_udp_peer_note(&t,&b,5)!=first);
 assert(!xip_udp_peer_get(&t,first,2+XIP_UDP_PEER_IDLE_US,&out));
 b=a;b.ip[4]=9;uint64_t recycled=xip_udp_peer_note(&t,&b,10+XIP_UDP_PEER_IDLE_US);assert(recycled!=first);assert(!xip_udp_peer_get(&t,first,11+XIP_UDP_PEER_IDLE_US,&out));
 t.epoch=8;assert(!xip_udp_peer_get(&t,recycled,12+XIP_UDP_PEER_IDLE_US,&out));
 xip_udp_peers same={.epoch=9};uint64_t old=xip_udp_peer_note(&same,&a,1);uint64_t renewed=xip_udp_peer_note(&same,&a,2+XIP_UDP_PEER_IDLE_US);assert(old!=renewed);assert(!xip_udp_peer_get(&same,old,3+XIP_UDP_PEER_IDLE_US,&out));assert(xip_udp_peer_get(&same,renewed,3+XIP_UDP_PEER_IDLE_US,&out));
}
int main(void){ownership();epoch_and_backpressure();completion_capacity();peers();puts("native endpoint ownership/epochs/UDP peers passed");}
