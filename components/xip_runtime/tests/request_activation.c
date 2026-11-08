/* Production numeric resolver and initializer; only loader/dispatch boundaries are fake. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>
#include "xip.h"
#define XIP_MAX_INSTANCES 4
#define QUIC_CONSOLE_COMPONENT 20
#define portMAX_DELAY -1
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
struct xip_instance {uint32_t id;int component;uint32_t port,stream;bool start_done;int envelope_components[XIP_COMPONENTS_MAX];struct{char name[16];}comps[XIP_COMPONENTS_MAX];const uint8_t *start_request;size_t start_len;};
static struct xip_instance *registry[XIP_MAX_INSTANCES],owner,console;
static int registry_lock,exec_lock,starts,published,loads,initialization_error,load_error;
static struct {int (*request_start)(int32_t,uint32_t);} hooks;
static void xSemaphoreTakeRecursive(int lock,int timeout){(void)lock;(void)timeout;}
static void xSemaphoreGiveRecursive(int lock){(void)lock;}
static struct xip_instance *by_component(int c,const struct xip_instance *except){for(unsigned i=0;i<4;i++)if(registry[i] && registry[i]!=except && registry[i]->component==c)return registry[i];return NULL;}
static int dispatch(struct xip_instance *in,int c,int m,int value,uint32_t flags,const uint8_t *data,size_t n){(void)in;(void)value;(void)flags;(void)data;(void)n;assert(c==XIP_HOST_COMPONENT && m==XIP_HOST_START);starts++;return initialization_error;}
static bool settle(struct xip_instance *in,int rc){if(!rc)return false;for(unsigned i=0;i<4;i++)if(registry[i]==in)registry[i]=NULL;return true;}
static void components_publish(struct xip_instance *in){assert(in->start_done);published++;}
/* ACTUAL_RESOLVER */
/* ACTUAL_INITIALIZER */
static int load(int32_t c,uint32_t port){loads++;if(load_error)return load_error;
    if(c==47 || c==52 || (!c && port==250)){owner=(struct xip_instance){.id=2,.component=47,.port=250,.envelope_components={52}};registry[1]=&owner;return 0;}
    return XIP_ERR_NOTFOUND;
}
int main(void){
    console=(struct xip_instance){.id=1,.component=20,.start_done=true};registry[0]=&console;hooks.request_start=load;int error=0;
    assert(request_target(47,0,&error)==&owner && !error && starts==1 && loads==1 && published==1);
    assert(request_target(52,0,&error)==&owner && starts==1 && loads==1);
    assert(request_target(0,250,&error)==&owner && starts==1);
    assert(instance_start(&owner) && starts==1);
    registry[1]=NULL;load_error=XIP_ERR_NOMEM;
    assert(!request_target(47,0,&error) && error==XIP_ERR_NOMEM && starts==1);
    load_error=0;initialization_error=XIP_ERR_UNSUPPORTED;
    assert(!request_target(47,0,&error) && error==XIP_ERR_UNSUPPORTED && starts==2 && published==1);
    initialization_error=0;
    assert(request_target(47,0,&error)==&owner && starts==3 && published==2);
    assert(request_target(999,0,&error)==&console && !error);
    assert(!request_target(0,999,&error) && error==XIP_ERR_NOTFOUND);
    return 0;
}
