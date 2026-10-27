#include <assert.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <stdio.h>
#include "xip.h"
#include "xip_pool.h"
#include "runtime_source_address.inc"
#include "xip_udp_peers.h"
#include "xip_quic.h"
static xip_quic_table quic_table;
static int64_t esp_timer_get_time(void){return 1000000;}
typedef int SemaphoreHandle_t;
static SemaphoreHandle_t quic_lock(void){return 1;}
static void xSemaphoreGive(SemaphoreHandle_t m){(void)m;}
static void xip_quic_classifier_enable(bool on);
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
#define pdTRUE 1
#define XIP_EV_PACKET 7
#define XIP_EV_LINK 8
static int registry_lock,worker_queue;
static struct {int (*request_start)(int32_t,uint32_t);} hooks;
typedef struct{uint8_t kind;uint32_t inst;int32_t value,component,method;uint64_t id;uint32_t from,flags;}xip_event_t;
struct xip_instance{uint32_t id;int component;struct{uint32_t pkt_sent;}stats;
 unsigned pkt_head,pkt_count;struct{int16_t slot;uint16_t start,len;int32_t component,method,value;uint32_t from,lflags,time_us;uint64_t id;}pkt_in[4];};
static struct xip_instance instance;
static xip_pool *pool;
static _Atomic uint32_t quic_module_owner;
static _Atomic bool quic_classifier;
static void xip_quic_classifier_enable(bool on){atomic_store(&quic_classifier,on);}
static unsigned udp_ensure_calls;
static void xip_quic_udp_ensure(void){udp_ensure_calls++;}
static int queue_accept=1;
static struct xip_instance *I(void *p){return p;}
static void drop_slot(int s){xip_pool_release(pool,s);}
static bool known_locked(struct xip_instance *p){return p==&instance;}
static struct xip_instance *find(uint32_t id){return instance.id==id?&instance:NULL;}
static int xQueueSend(int q,const xip_event_t *event,int wait){(void)q;(void)event;(void)wait;return queue_accept;}
#include "runtime_delivery_config.inc"
#ifdef TEST_GLOBAL_PACKET_QUEUE
#define CONFIG_XIP_PACKET_STATS 0
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-function"
#include "packet_queue.inc"
#pragma GCC diagnostic pop
static packet_delivery entries[4];
static packet_delivery_queue packet_queue={.entries=entries,.capacity=4};
#endif
#include "runtime_deliver.inc"
typedef struct{int link;const char *name;uint64_t source;int32_t signal;uint32_t time_us,port;}xip_rx_info;
typedef struct { uint8_t *data; size_t len,headroom; int slot; xip_pool *pool; } xip_link_packet;
static int relay_link=-1;
static int xip_quic_udp_destination(const uint8_t address[22],uint64_t *token){(void)address;(void)token;return XIP_ERR_UNSUPPORTED;}
static int xip_link_native_unregister(int link){(void)link;return 0;}
static int link_tx(uint32_t link,int slot,uint16_t start,uint16_t len,uint64_t token,int sender,int component,int method)
{(void)link;(void)slot;(void)start;(void)len;(void)token;(void)sender;(void)component;(void)method;return XIP_ERR_BUSY;}
static int xip_link_native_register(const char *name,uint32_t mtu,uint32_t flags,int (*tx)(void*,uint64_t,const xip_link_packet*),void *ctx)
{(void)name;(void)mtu;(void)flags;(void)tx;(void)ctx;return 65539;}
static int xip_link_native_set_state(int link,uint32_t state,uint32_t rate)
{(void)link;(void)state;(void)rate;return 0;}
#include "xip_quic_module.inc"
static void ownership(void)
{
 pool=xip_pool_create(32,1704);assert(pool);
 instance.id=1;instance.component=48;
 assert(quic_module_table()->size==sizeof(xip_quic));
 assert(c_quic_attach(&instance,100)==0);assert(udp_ensure_calls==1);
 quic_module_event(65537,1,0,0);
 assert(!atomic_load(&quic_classifier));assert(atomic_load(&quic_module_owner)==1);
 xip_rx_info info={.link=65537,.name="udp",.source=91,.signal=-43,.time_us=123};
 for(int i=0;i<5;i++){
   int slot=xip_pool_acquire(pool,0);assert(slot>=0);
   assert(quic_module_offer(&info,slot,144,8));
 }
#ifdef TEST_GLOBAL_PACKET_QUEUE
 assert(packet_queue.count==4 && xip_pool_available(pool)==28);
 packet_delivery packet;
 while(packet_queue_pop(&packet_queue,&packet)){
   assert(packet.value==-43 && packet.time_us==123);drop_slot(packet.slot);
 }
#else
 assert(instance.pkt_count==4);assert(xip_pool_available(pool)==28);
 for(int i=0;i<4;i++){
   assert(instance.pkt_in[i].value==-43 && instance.pkt_in[i].time_us==123);
   drop_slot(instance.pkt_in[i].slot);
 }
 instance.pkt_count=0;
#endif
 /* A driver owns its reference independently of endpoint registration. */
 int physical=xip_pool_acquire(pool,0);assert(physical>=0);
 assert(c_quic_detach(&instance)==0);assert(!atomic_load(&quic_module_owner));
 instance.id=2;assert(c_quic_attach(&instance,100)==0);
 assert(xip_pool_refs(pool,physical)==1);drop_slot(physical);
 c_quic_detach(&instance);assert(xip_pool_available(pool)==32);xip_pool_destroy(pool);pool=NULL;
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
int main(void){ownership();peers();puts("native endpoint registration/RX ownership/UDP peers passed");}
