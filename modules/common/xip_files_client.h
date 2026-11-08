/* Asynchronous program loading through the runtime-routed files service. */
#pragma once
#include "xip_module.h"
#define XIP_FILES_LOAD_TIMEOUT 2147418101

typedef struct xip_file_load {
    xip_call request;
    uint8_t *data;
    uint32_t size, offset, version;
    uint8_t name[8], name_len;
    uint64_t token;
    int alarm;
    uint8_t fields[];
} xip_file_load;

static inline void xip_file_error(const xip_host *host, const xip_call *call, int error)
{
    uint32_t pkt=0; uint8_t *p; size_t cap;
    if(host->pkt_alloc(host->user,&pkt))return;
    if(host->pkt_buf(host->user,pkt,&p,&cap)){host->pkt_free(host->user,pkt);return;}
    xip_cbor_w w;xip_cbor_init(&w,p,cap);xip_cbor_int(&w,error);
    int n=xip_cbor_done(&w);
    if(n<0){host->pkt_free(host->user,pkt);return;}
    host->send(host->user,pkt,(size_t)n,call->from,call->component,call->method,call->id);
}
static inline void xip_file_load_free(const xip_host *host,xip_file_load *load)
{
    if(!load)return;
    if(load->alarm>0)host->alarm_cancel(host->user,load->alarm);
    if(load->data)host->mem_free(host->user,load->data);
    host->mem_free(host->user,load);
}
static inline int xip_file_load_next(const xip_host *host,xip_file_load *load)
{
    uint32_t pkt=0;uint8_t *p;size_t cap;
    int rc=host->pkt_alloc(host->user,&pkt);if(rc<0)return rc;
    rc=host->pkt_buf(host->user,pkt,&p,&cap);
    if(rc<0){host->pkt_free(host->user,pkt);return rc;}
    xip_cbor_w w;xip_cbor_init(&w,p,cap);
    xip_cbor_map(&w,load->version?4:3);
    xip_cbor_uint(&w,1);xip_cbor_bytes(&w,load->name,load->name_len);
    xip_cbor_uint(&w,6);xip_cbor_uint(&w,load->offset);
    xip_cbor_uint(&w,7);xip_cbor_uint(&w,XIP_FILES_LOCAL_MAX);
    if(load->version){xip_cbor_uint(&w,5);xip_cbor_uint(&w,load->version);}
    int n=xip_cbor_done(&w);
    if(n<0){host->pkt_free(host->user,pkt);return XIP_ERR_SIZE;}
    return host->send(host->user,pkt,(size_t)n,XIP_FILES_COMPONENT,XIP_FILES_COMPONENT,XIP_FILES_GET,load->token);
}
static inline int xip_file_load_start(const xip_host *host,const xip_call *call,xip_file_load **out)
{
    xip_cbor_r r;xip_cbor_item item;
    if(*out)return XIP_ERR_BUSY;
    if(xip_cbor_get(call->data,call->len,1,&r)||xip_cbor_next(&r,&item)||item.type!=XIP_CBOR_BYTES||!item.n||item.n>8)return XIP_ERR_ARG;
    xip_file_load *load=host->mem_alloc(host->user,sizeof(*load)+call->len);
    if(!load)return XIP_ERR_NOMEM;
    *load=(xip_file_load){0};load->request=*call;
    xip_copy(load->fields,call->data,call->len);load->request.data=load->fields;load->request.pkt=0;
    xip_copy(load->name,item.data,item.n);load->name_len=(uint8_t)item.n;
    load->token=call->id^UINT64_C(0xa15f000000000000);
    load->alarm=host->alarm_set(host->user,30000,0,0,0,call->component,XIP_FILES_LOAD_TIMEOUT);
    if(load->alarm<0){int rc=load->alarm;xip_file_load_free(host,load);return rc;}
    int rc=xip_file_load_next(host,load);
    if(rc<0){xip_file_load_free(host,load);return rc;}
    *out=load;return 0;
}
/* One means complete, zero means another range is pending, negative is an error. */
static inline int xip_file_load_receive(const xip_host *host,xip_file_load *load,const xip_call *call)
{
    xip_cbor_r r;xip_cbor_item item;int64_t error;
    xip_cbor_reader(&r,call->data,call->len);
    if(!xip_cbor_next(&r,&item)&&!xip_cbor_as_int(&item,&error)&&error<0)return (int)error;
    int64_t version,size,offset;
    if(xip_cbor_get_int(call->data,call->len,1,&version)||version<=0||version>UINT32_MAX||
       xip_cbor_get_int(call->data,call->len,2,&size)||size<0||size>XIP_FILES_MAX_FILE||
       xip_cbor_get_int(call->data,call->len,6,&offset)||offset!=load->offset||
       xip_cbor_get(call->data,call->len,13,&r)||xip_cbor_next(&r,&item)||item.type!=XIP_CBOR_BYTES)return XIP_ERR_IO;
    if(!load->data){
        load->size=(uint32_t)size;load->version=(uint32_t)version;
        load->data=host->mem_alloc(host->user,size?(size_t)size:1);
        if(!load->data)return XIP_ERR_NOMEM;
    }
    if((uint32_t)size!=load->size||(uint32_t)version!=load->version||item.n>load->size-load->offset||(!item.n&&load->offset<load->size))return XIP_ERR_IO;
    xip_copy(load->data+load->offset,item.data,item.n);load->offset+=(uint32_t)item.n;
    if(load->offset==load->size)return 1;
    return xip_file_load_next(host,load);
}
