/* Serialized, generation-bound module streams. No socket or body buffer. */
typedef struct module_stream {
    struct module_stream *next;
    uint32_t handle,provider,consumer;
    int component,method;
    uint64_t request_id;
    bool has_id,fin,pending,out_fin;
    int output_slot;uint32_t output_len;
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
    free(s);
}
static void streams_release(uint32_t owner)
{
    stream_service **p=&stream_services;while(*p){if((*p)->owner==owner){stream_service *gone=*p;*p=gone->next;free(gone);}else p=&(*p)->next;}

    module_stream *s=module_streams;
    while(s){module_stream *next=s->next;if(s->provider==owner || s->consumer==owner)stream_remove(s,owner);s=next;}
}
static int c_stream_open(void *ctx,const uint8_t *data,size_t len)
{
    struct xip_instance *in=I(ctx);xip_envelope env;
    if(xip_envelope_parse(data,len,&env)!=XIP_ENV_REQUEST)return XIP_ERR_ARG;
    struct xip_instance *t=by_component(env.component,in);
    bool declared=t && t->stream;
    if(!declared)for(stream_service *p=stream_services;p;p=p->next)
        if(p->component==env.component && p->method==env.method){t=find(p->owner);declared=true;break;}
    if(!t || !declared)return XIP_ERR_NOTFOUND;
    unsigned count=0;for(module_stream *s=module_streams;s;s=s->next)count++;
    if(count>=8)return XIP_ERR_BUSY;
    module_stream *s=calloc(1,sizeof(*s));if(!s)return XIP_ERR_NOMEM;
    if(stream_sequence==INT32_MAX){free(s);return XIP_ERR_BUSY;}
    *s=(module_stream){.handle=++stream_sequence,.provider=in->id,.consumer=t->id,
        .component=env.component,.method=env.method,
        .request_id=env.id,.has_id=env.has_id,.output_slot=-1,.next=module_streams};
    module_streams=s;
    const uint32_t handle=s->handle,consumer=s->consumer;
    int rc=stream_call(s,XIP_STREAM_OPEN,env.fields,env.fields_len);
    s=stream_find(handle); /* consumer may close during the inline callback */
    if(rc<0){if(s)stream_remove(s,consumer);return rc;}
    return s?(int)handle:XIP_ERR_NOTFOUND;
}
static int c_stream_feed(void *ctx,uint32_t handle,const uint8_t *data,size_t len,uint32_t fin)
{
    module_stream *s=stream_find(handle);
    if(!s || s->provider!=I(ctx)->id)return XIP_ERR_NOTFOUND;
    if((!data && len) || (fin && len) || len>INT32_MAX || s->fin)return XIP_ERR_ARG;
    int rc=len?stream_call(s,XIP_STREAM_DATA,data,len):0;
    if(rc<0 || (size_t)rc>len)return rc<0?rc:XIP_ERR_ARG;
    if((size_t)rc==len && fin){
        s=stream_find(handle);if(!s)return XIP_ERR_NOTFOUND;
        s->fin=true; /* callback may close/free the row */
        int end=stream_call(s,XIP_STREAM_FIN,NULL,0);if(end<0)return end;
    }
    return rc;
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
    xip_held_entry *h=xip_held_get(&in->held,packet);
    if(!h || h->state!=XIP_HELD_OWNED || len>XIP_PKT_MAX)return XIP_ERR_ARG;
    if(!find(s->provider))return XIP_ERR_NOTFOUND;
    xip_held_entry moved=xip_held_take(&in->held,packet);
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
    if(xip_held_count(&in->held,XIP_HELD_OWNED)+2>=in->held.quota)return XIP_ERR_BUSY;
    uint32_t h=xip_held_add(&in->held,s->output_slot,XIP_PKT_HEADROOM,s->output_len,XIP_HELD_OWNED);
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
