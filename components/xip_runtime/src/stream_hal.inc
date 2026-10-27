/* Serialized, generation-bound module streams. No socket or body buffer. */
typedef struct module_stream {
    struct module_stream *next;
    uint32_t handle,provider,consumer;
    int component,method;
    uint64_t request_id;
    bool has_id,fin,pending,out_fin;
    int output_slot;uint32_t output_len;
    uint8_t *header;uint16_t header_len,header_cap;
} module_stream;
typedef struct stream_service {struct stream_service *next;uint32_t owner;int component,method;} stream_service;
static stream_service *stream_services;
static int c_stream_listen(void *ctx,int component,int method)
{
    if(component<=0 || method<=0)return XIP_ERR_ARG;
    if(by_component(component,I(ctx)))return XIP_ERR_BUSY;
    unsigned n=0;
    for(stream_service *p=stream_services;p;p=p->next){
        n++;if(p->component==component && p->method==method)return p->owner==I(ctx)->id?XIP_OK:XIP_ERR_BUSY;
    }
    if(n>=8)return XIP_ERR_BUSY;
    stream_service *p=malloc(sizeof(*p));if(!p)return XIP_ERR_NOMEM;
    *p=(stream_service){.owner=I(ctx)->id,.component=component,.method=method,.next=stream_services};stream_services=p;return XIP_OK;
}
static module_stream *module_streams;
static uint32_t stream_sequence;
static module_stream *stream_find(uint32_t handle)
{ for(module_stream *s=module_streams;s;s=s->next)if(s->handle==handle)return s;return NULL; }
static int stream_call(module_stream *s,int event,const uint8_t *data,size_t len)
{
    struct xip_instance *t=find(s->consumer);
    if(!t)return XIP_ERR_NOTFOUND;
    struct xip_instance *provider=find(s->provider);
    xip_call call={.size=sizeof(call),.component=s->component,.method=s->method,
        .value=event,.flags=XIP_CALL_STREAM,.id=s->handle,.from=provider?(uint32_t)provider->component:0,
        .data=data,.len=len,.request_id=s->request_id,.request_has_id=s->has_id};
    /* Return value is consumption, not RET_DONE: never pass to settle(). */
    return dispatch_x(t,&call);
}
static void stream_remove(module_stream *s,uint32_t initiator)
{
    module_stream **p=&module_streams;while(*p && *p!=s)p=&(*p)->next;
    if(!*p)return;
    *p=s->next;
    if(initiator!=s->consumer)(void)stream_call(s,XIP_STREAM_CLOSED,NULL,0);
    /* Provider notices a stopped consumer via feed/written and normal tick.
     * No callback may enter the provider while it is already progressing QUIC. */
    if(s->output_slot>=0)drop_slot(s->output_slot);
    free(s->header);free(s);
}
static void streams_release(uint32_t owner)
{
    stream_service **p=&stream_services;while(*p){if((*p)->owner==owner){stream_service *gone=*p;*p=gone->next;free(gone);}else p=&(*p)->next;}

    module_stream *s=module_streams;
    while(s){module_stream *next=s->next;if(s->provider==owner || s->consumer==owner)stream_remove(s,owner);s=next;}
}
/* Definite CBOR prefix framing. Enforce the same 512-byte opening bound for
 * every stream provider; body bytes are borrowed directly, never buffered here. */
#define STREAM_HEADER_MAX 512
static int stream_header_ready(const uint8_t *data,size_t len)
{
    if(!len)return 0;
    if((data[0]>>5)!=5)return XIP_ERR_ARG;
    size_t at=0,pending=1;
    while(pending){
        if(at==len)return 0;
        unsigned b=data[at++],major=b>>5,info=b&31;
        if(major==6 || info>=28 || (major==7 && (info<20 || info>23)))return XIP_ERR_ARG;
        uint64_t arg=info;
        if(major!=7 && info>=24){
            unsigned extra=1u<<(info-24);
            if(extra>len-at)return 0;
            arg=0;for(unsigned i=0;i<extra;i++)arg=(arg<<8)|data[at++];
        }
        pending--;
        if(major==2 || major==3){
            if(arg>STREAM_HEADER_MAX)return XIP_ERR_SIZE;
            if(arg>len-at)return 0;
            at+=(size_t)arg;
        }else if(major==4 || major==5){
            unsigned multiple=major==5?2:1;
            if(arg>(STREAM_HEADER_MAX-pending)/multiple)return XIP_ERR_SIZE;
            pending+=(size_t)arg*multiple;
        }
    }
    return at==len?1:XIP_ERR_ARG;
}
static int stream_bind(module_stream *s,const uint8_t *data,size_t len)
{
    struct xip_instance *in=find(s->provider);xip_envelope env;
    if(!in || xip_envelope_parse(data,len,&env)!=XIP_ENV_REQUEST)return XIP_ERR_ARG;
    int activation=0;
    /* An explicitly registered stream service wins over the fallback below:
     * `request_target` answers an unhosted component with the console, and the
     * console's stream table would otherwise capture every stream — including
     * the discovery commands a module registered (component 6). A module
     * without a TCP listener has a NULL `stream` (the manifest's key 10 port),
     * so the registry path must not test it. */
    struct xip_instance *t=NULL;
    for(stream_service *p=stream_services;p;p=p->next)
        if(p->component==env.component && p->method==env.method){t=find(p->owner);break;}
    if(!(t && t!=in)){
        t=request_target(env.component,0,&activation);
        if(!(t && t!=in && t->stream))return activation<0?activation:XIP_ERR_NOTFOUND;
    }
    s->consumer=t->id;s->component=env.component;s->method=env.method;
    s->request_id=env.id;s->has_id=env.has_id;
    return stream_call(s,XIP_STREAM_OPEN,env.fields,env.fields_len);
}
static int c_stream_open(void *ctx,const uint8_t *data,size_t len)
{
    struct xip_instance *in=I(ctx);
    if((!data && len) || (data && !len) || len>STREAM_HEADER_MAX)return XIP_ERR_ARG;
    unsigned count=0;for(module_stream *s=module_streams;s;s=s->next)count++;
    if(count>=8)return XIP_ERR_BUSY;
    module_stream *s=calloc(1,sizeof(*s));if(!s)return XIP_ERR_NOMEM;
    if(stream_sequence==INT32_MAX){free(s);return XIP_ERR_BUSY;}
    *s=(module_stream){.handle=++stream_sequence,.provider=in->id,
        .output_slot=-1,.next=module_streams};
    module_streams=s;
    const uint32_t handle=s->handle;
    int rc=data?stream_bind(s,data,len):0;
    s=stream_find(handle); /* consumer may close during the inline callback */
    if(rc<0){if(s)stream_remove(s,s->consumer);return rc;}
    return s?(int)handle:XIP_ERR_NOTFOUND;
}
static int c_stream_feed(void *ctx,uint32_t handle,const uint8_t *data,size_t len,uint32_t fin)
{
    module_stream *s=stream_find(handle);
    if(!s || s->provider!=I(ctx)->id)return XIP_ERR_NOTFOUND;
    if((!data && len) || (fin && len) || len>INT32_MAX || s->fin)return XIP_ERR_ARG;
    size_t used=0;
    while(!s->consumer){
        if(used==len)return fin?XIP_ERR_ARG:(int)used;
        if(s->header_len==STREAM_HEADER_MAX)return XIP_ERR_SIZE;
        if(s->header_len==s->header_cap){
            unsigned cap=s->header_cap?s->header_cap*2:32;
            uint8_t *h=realloc(s->header,cap);if(!h)return used?(int)used:XIP_ERR_NOMEM;
            s->header=h;s->header_cap=cap;
        }
        s->header[s->header_len++]=data[used++];
        int ready=stream_header_ready(s->header,s->header_len);
        if(ready<0)return ready;
        if(!ready)continue;
        /* Detach framing storage before OPEN: that callback may free the row. */
        uint8_t *header=s->header;size_t header_len=s->header_len;
        s->header=NULL;s->header_len=s->header_cap=0;
        int rc=stream_bind(s,header,header_len);free(header);
        s=stream_find(handle);
        if(rc<0){if(s)stream_remove(s,s->consumer);return rc;}
        if(!s)return XIP_ERR_NOTFOUND;
    }
    size_t body=len-used;
    int rc=body?stream_call(s,XIP_STREAM_DATA,data+used,body):0;
    if(rc<0 || (size_t)rc>body)return rc<0?rc:XIP_ERR_ARG;
    if((size_t)rc==body && fin){
        s=stream_find(handle);if(!s)return XIP_ERR_NOTFOUND;
        s->fin=true; /* callback may close/free the row */
        int end=stream_call(s,XIP_STREAM_FIN,NULL,0);if(end<0)return end;
    }
    return (int)used+rc;
}
static int c_stream_close(void *ctx,uint32_t handle)
{
    module_stream *s=stream_find(handle);uint32_t owner=I(ctx)->id;
    if(!s || (s->provider!=owner && s->consumer!=owner))return XIP_ERR_NOTFOUND;
    stream_remove(s,owner);return XIP_OK;
}
static int c_stream_write(void *ctx,uint32_t handle,uint32_t packet,uint32_t len,uint32_t fin)
{
    struct xip_instance *in=I(ctx);module_stream *s=stream_find(handle);
    if(!s || s->consumer!=in->id)return XIP_ERR_NOTFOUND;
    if(s->pending)return XIP_ERR_BUSY;
    if(s->out_fin)return XIP_ERR_ARG;
    xip_packet_ref *h=xip_packet_get(pool,packet);
    if(!h || len>XIP_PKT_MAX)return XIP_ERR_ARG;
    if(!find(s->provider))return XIP_ERR_NOTFOUND;
    xip_packet_ref moved=packet_take(packet);
    s->output_slot=moved.slot;s->output_len=len;s->pending=true;s->out_fin=fin!=0;
    return XIP_OK;
}
static int c_stream_take(void *ctx,uint32_t handle,xip_stream_packet *out)
{
    struct xip_instance *in=I(ctx);module_stream *s=stream_find(handle);
    if(!s || s->provider!=in->id)return XIP_ERR_NOTFOUND;
    if(!out)return XIP_ERR_ARG;
    if(s->output_slot<0)return 0;
    /* Preserve one ingress lease and one returned physical TX lease. */
    uint32_t h=xip_packet_handle(pool,s->output_slot,XIP_PKT_HEADROOM,s->output_len);
    if(!h)return XIP_ERR_BUSY;
    *out=(xip_stream_packet){.packet=h,.len=s->output_len,.fin=s->out_fin};s->output_slot=-1;
    return 1;
}
static int c_stream_written(void *ctx,uint32_t handle)
{
    module_stream *s=stream_find(handle);
    if(!s || s->provider!=I(ctx)->id)return XIP_ERR_NOTFOUND;
    s->pending=false;return XIP_OK;
}
static const xip_stream module_stream_table={.version=1,.size=sizeof(xip_stream),
    .listen=c_stream_listen,.open=c_stream_open,.feed=c_stream_feed,.close=c_stream_close,.write_pkt=c_stream_write,.take=c_stream_take,.written=c_stream_written};
