/* Real broker + real pool; only module callbacks/registry are faked. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include "xip.h"
#include "xip_pool.h"
#include "xip_envelope.h"
#define XIP_PKT_HEADROOM 144
#define XIP_PKT_MAX 1500
#define XIP_MAX_INSTANCES 4
#define QUIC_CONSOLE_COMPONENT 20
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
struct xip_instance {uint32_t id;int component,stream,port;
    int envelope_components[XIP_COMPONENTS_MAX];struct {char name[16];} comps[XIP_COMPONENTS_MAX];};
static struct xip_instance provider={.id=1,.component=48},consumer={.id=2,.component=44,.stream=2,.envelope_components={1}};
static struct xip_instance console={.id=4,.component=20};
static struct xip_instance *registry[XIP_MAX_INSTANCES]={&provider,&consumer,&console};
static int registry_lock;
static xip_pool *pool;
static const uint8_t *expected;
static unsigned opens,ends,closed;
static uint64_t expected_id=17;
static bool close_on_open,close_on_fin;
static int c_stream_close(void *ctx,uint32_t handle);
static struct xip_instance *I(void *p){return p;}
static struct xip_instance *find(uint32_t id){return id==provider.id?&provider:id==consumer.id?&consumer:NULL;}
static struct xip_instance *by_component(int c,const struct xip_instance *except){
    for(unsigned i=0;i<XIP_MAX_INSTANCES;i++)if(registry[i] && registry[i]!=except && registry[i]->component==c)return registry[i];
    return NULL;
}
#include "envelope_target.inc"
static struct xip_instance *request_target(int c,uint32_t port,int *error){(void)port;struct xip_instance *t=envelope_target(c);if(!t)t=by_component(20,NULL);*error=t?0:XIP_ERR_NOTFOUND;return t;}
static void drop_slot(int s){xip_pool_release(pool,s);}
static int dispatch_x(struct xip_instance *in,const xip_call *call){
    assert(in==&consumer && (((call->component==44 || call->component==1) && call->method==1) || (call->component==8 && call->method==2)));
    assert(call->flags==XIP_CALL_STREAM && call->request_id==expected_id && call->request_has_id);
    switch(call->value){
    case XIP_STREAM_OPEN:opens++;if(close_on_open)c_stream_close(in,(uint32_t)call->id);return 0;
    case XIP_STREAM_DATA:assert(call->data==expected);return call->len>2?2:(int)call->len;
    case XIP_STREAM_FIN:ends++;if(close_on_fin)c_stream_close(in,(uint32_t)call->id);return 0;
    case XIP_STREAM_CLOSED:closed++;return 0;
    default:abort();
    }
}
static xip_packet_ref packet_take(uint32_t p){return *xip_packet_get(pool,p);}
#include "stream_hal.inc"
static uint32_t packet(struct xip_instance *in){
    int slot=xip_pool_acquire(pool,0);assert(slot>=0);
    memcpy(xip_pool_slot(pool,slot)+144,"body",4);
    (void)in;uint32_t h=xip_packet_handle(pool,slot,144,4);assert(h);return h;
}
static void release(struct xip_instance *in,uint32_t h){(void)in;xip_packet_ref e=packet_take(h);drop_slot(e.slot);}
int main(void){
    pool=xip_pool_create(12,1704);assert(pool);
    uint8_t env[]={0xa4,1,0x18,44,2,1,3,17,5,0xa0};
    int id=c_stream_open(&provider,env,sizeof(env));assert(id>0 && opens==1);
    uint8_t body[]={1,2,3,4};expected=body;
    assert(c_stream_feed(&provider,id,body,4,0)==2);
    expected=body+2;assert(c_stream_feed(&provider,id,body+2,2,0)==2);
    assert(c_stream_feed(&provider,id,NULL,0,1)==0 && ends==1);
    assert(c_stream_feed(&provider,id,NULL,0,1)==XIP_ERR_ARG);
    uint32_t h=packet(&consumer);int slot=xip_packet_get(pool,h)->slot;
    uint8_t *pointer=xip_pool_slot(pool,slot);
    assert(c_stream_write(&provider,id,h,4,1)==XIP_ERR_NOTFOUND);
    assert(xip_packet_get(pool,h));
    assert(c_stream_write(&consumer,id,h,4,1)==0);
    assert(xip_packet_get(pool,h));
    uint32_t other=packet(&consumer);
    assert(c_stream_write(&consumer,id,other,4,1)==XIP_ERR_BUSY && xip_packet_get(pool,other));
    uint32_t pressure[2]={packet(&provider),packet(&provider)};
    xip_stream_packet out={0};
    release(&provider,pressure[0]);release(&provider,pressure[1]);
    assert(c_stream_take(&provider,id,&out)==1 && out.len==4 && out.fin==1);
    assert(xip_packet_get(pool,out.packet)->slot==slot);
    assert(pointer==xip_pool_slot(pool,slot) && xip_pool_refs(pool,slot)==1);
    assert(c_stream_take(&provider,id,&out)==0);
    release(&provider,out.packet);release(&consumer,other);
    assert(c_stream_written(&provider,id)==0);
    assert(c_stream_close(&provider,id)==0 && closed==1);
    assert(c_stream_feed(&provider,id,body,4,0)==XIP_ERR_NOTFOUND);
    int next=c_stream_open(&provider,env,sizeof(env));assert(next>id);
    h=packet(&consumer);assert(c_stream_write(&consumer,next,h,4,0)==0);
    streams_release(consumer.id);consumer.id=3;
    assert(c_stream_take(&provider,next,&out)==XIP_ERR_NOTFOUND);
    assert(xip_pool_available(pool)==12 && !module_streams);
    next=c_stream_open(&provider,env,sizeof(env));assert(next>id);
    streams_release(provider.id);assert(closed==2 && !module_streams);
    assert(c_stream_listen(&consumer,8,2)==0);
    assert(c_stream_listen(&provider,8,2)==XIP_ERR_BUSY);
    consumer.stream=0; /* a bound QUIC-only service needs no TCP listener */
    uint8_t alias[]={0xa4,1,8,2,2,3,17,5,0xa0};
    next=c_stream_open(&provider,alias,sizeof(alias));assert(next>0);
    assert(c_stream_close(&provider,next)==0);
    streams_release(consumer.id);assert(!stream_services);
    assert(c_stream_open(&provider,alias,sizeof(alias))==XIP_ERR_NOTFOUND);
    consumer.stream=2;
    close_on_open=true;assert(c_stream_open(&provider,env,sizeof(env))==XIP_ERR_NOTFOUND);assert(!module_streams);close_on_open=false;
    next=c_stream_open(&provider,env,sizeof(env));assert(next>0);
    close_on_fin=true;assert(c_stream_feed(&provider,next,NULL,0,1)==0);assert(!module_streams);close_on_fin=false;
    /* Every split, including inside integer/string/container heads, belongs to
     * the runtime. Coalesced body is passed by its original pointer. */
    uint8_t opening[]={0xa4,1,0x18,44,2,1,3,17,5,0xa1,1,0x63,'a','b','c',9,8};
    for(size_t split=0;split<=15;split++){
        unsigned before=opens;
        next=c_stream_open(&provider,NULL,0);assert(next>0 && opens==before);
        assert(c_stream_feed(&provider,next,opening,split,0)==(int)split);
        assert(opens==before+(split==15));
        expected=opening+15;
        assert(c_stream_feed(&provider,next,opening+split,sizeof(opening)-split,0)==(int)(sizeof(opening)-split));
        assert(opens==before+1);
        assert(c_stream_feed(&provider,next,NULL,0,1)==0);
        assert(c_stream_close(&provider,next)==0 && !module_streams);
    }
    next=c_stream_open(&provider,NULL,0);
    for(size_t i=0;i<15;i++)assert(c_stream_feed(&provider,next,opening+i,1,0)==1);
    assert(c_stream_close(&provider,next)==0);
    next=c_stream_open(&provider,NULL,0);
    assert(c_stream_feed(&provider,next,opening,4,0)==4);
    assert(c_stream_feed(&provider,next,NULL,0,1)==XIP_ERR_ARG);
    streams_release(provider.id);assert(!module_streams);
    uint8_t invalid[]={0xbf};next=c_stream_open(&provider,NULL,0);
    assert(c_stream_feed(&provider,next,invalid,sizeof(invalid),0)==XIP_ERR_ARG);
    c_stream_close(&provider,next);
    uint8_t oversized[]={0xa1,1,0x59,0x02,0x01};next=c_stream_open(&provider,NULL,0);
    assert(c_stream_feed(&provider,next,oversized,sizeof(oversized),0)==XIP_ERR_SIZE);
    c_stream_close(&provider,next);
    uint8_t policy[]={0xa3,1,1,2,1,3,17};
    next=c_stream_open(&provider,NULL,0);
    assert(c_stream_feed(&provider,next,policy,sizeof(policy),0)==sizeof(policy));
    assert(c_stream_close(&provider,next)==0);
    next=c_stream_open(&provider,NULL,0);close_on_open=true;
    assert(c_stream_feed(&provider,next,env,sizeof(env),0)==XIP_ERR_NOTFOUND);
    assert(!module_streams);close_on_open=false;
    uint8_t correlated[]={0xa3,1,0x18,44,2,1,3,0x1b,0,6,0x5e,0x72,0x34,0x56,0x78,0x9a};
    expected_id=0x00065e723456789aULL;
    next=c_stream_open(&provider,NULL,0);
    for(size_t i=0;i<sizeof(correlated);i++)assert(c_stream_feed(&provider,next,correlated+i,1,0)==1);
    assert(c_stream_close(&provider,next)==0);expected_id=17;
    for(unsigned i=0;i<8;i++)assert(c_stream_open(&provider,NULL,0)>0);
    assert(c_stream_open(&provider,NULL,0)==XIP_ERR_BUSY);
    streams_release(provider.id);assert(!module_streams);
    assert(xip_pool_available(pool)==12);
    xip_pool_destroy(pool);return 0;
}
