/* Endpoint registration only. Pool references belong to their current holders;
 * native drivers release accepted TX independently of endpoint lifetime. */
struct quic_module_state { uint32_t owner; int component,rx_method; };
static struct quic_module_state *quic_module;
static int c_quic_attach(void *ctx,int rx_method)
{
    struct xip_instance *in=I(ctx);
    if(rx_method<=0)return XIP_ERR_ARG;
    if(quic_module)return quic_module->owner==in->id?XIP_OK:XIP_ERR_BUSY;
    struct quic_module_state *s=calloc(1,sizeof(*s));
    if(!s)return XIP_ERR_NOMEM;
    *s=(struct quic_module_state){in->id,in->component,rx_method};
    portENTER_CRITICAL(&registry_lock);
    quic_module=s;atomic_store(&quic_module_owner,in->id);
    portEXIT_CRITICAL(&registry_lock);
    xip_quic_udp_ensure();return XIP_OK;
}
static void quic_module_release(struct xip_instance *in)
{
    portENTER_CRITICAL(&registry_lock);
    struct quic_module_state *s=quic_module;
    if(!s || s->owner!=in->id){portEXIT_CRITICAL(&registry_lock);return;}
    quic_module=NULL;atomic_store(&quic_module_owner,0);
    portEXIT_CRITICAL(&registry_lock);free(s);
}
static int c_quic_detach(void *ctx){quic_module_release(I(ctx));return XIP_OK;}
static bool quic_module_offer(const xip_rx_info *info,int slot,uint16_t start,uint16_t len)
{
    uint32_t owner=0;int component=0,method=0;
    portENTER_CRITICAL(&registry_lock);
    if(quic_module){owner=quic_module->owner;component=quic_module->component;method=quic_module->rx_method;}
    portEXIT_CRITICAL(&registry_lock);
    if(!owner)return protocol_on_demand(slot,start,len,(uint32_t)info->link,info->source,info->signal,info->time_us);
    struct xip_instance *in=find(owner);
    if(deliver_checked(in,owner,slot,start,len,component,method,(uint32_t)info->link,info->source,
                       info->signal,XIP_CALL_LINK,info->time_us,NULL))drop_slot(slot);
    return true;
}
static void quic_module_event(uint32_t addr,uint32_t kind,int32_t value,uint64_t id)
{
    xip_event_t ev={.kind=XIP_EV_LINK,.value=value,.id=id,.from=addr,
        .flags=XIP_CALL_LINK|XIP_CALL_LINK_EVENT|(kind<<8)};
    portENTER_CRITICAL(&registry_lock);
    if(quic_module){ev.inst=quic_module->owner;ev.component=quic_module->component;ev.method=quic_module->rx_method;}
    portEXIT_CRITICAL(&registry_lock);
    if(ev.inst)(void)xQueueSend(worker_queue,&ev,0);
}
/* A native virtual link makes the circuit an ordinary endpoint bearer.
 * Its token is the local endpoint DCID; reverse hops are explicitly installed. */
static int relay_tx(void *ctx,uint64_t token,const xip_link_packet *pkt)
{
    (void)ctx;
    uint8_t cid[8];memcpy(cid,&token,8);
    unsigned n=xip_quic_cid_class_len(cid[0]);
    xip_quic_entry e={0};SemaphoreHandle_t m=quic_lock();
    int rc=xip_quic_relay_select_at(&quic_table,cid,n,(uint64_t)esp_timer_get_time(),&e);xSemaphoreGive(m);
    if(rc || e.kind!=3)return XIP_ERR_NOTFOUND;
    size_t prefix=2+e.out_len;
    if(pkt->headroom<prefix || pkt->len+prefix>XIP_QUIC_MAX_PACKET)return XIP_ERR_SIZE;
    uint8_t *p=(uint8_t *)pkt->data-prefix;p[0]=0x40;memcpy(p+1,e.out_cid,e.out_len);p[1+e.out_len]=0;
    if(e.out_address_len){rc=xip_quic_udp_destination(e.out_address,&e.token);if(rc<0)return rc;}
    return link_tx(e.egress,pkt->slot,(uint16_t)(pkt->headroom-prefix),(uint16_t)(pkt->len+prefix),e.token,0,0,0);
}
static int relay_link_ensure(void)
{
    if(relay_link>=0)return 0;
    int link=xip_link_native_register("relay",XIP_QUIC_MAX_PACKET-22,0,relay_tx,NULL);
    if(link<0)return link;
    relay_link=link;xip_link_native_set_state(link,XIP_LINK_UP,0);return 0;
}
static void relay_link_gc(void)
{
    if(relay_link<0)return;
    bool used=false;SemaphoreHandle_t m=quic_lock();
    for(unsigned i=0;i<quic_table.n;i++)if(quic_table.e[i].route && quic_table.e[i].route->endpoint){used=true;break;}
    xSemaphoreGive(m);
    if(!used){int link=relay_link;relay_link=-1;xip_link_native_unregister(link);}
}
static int c_relay_set(void *ctx,const xip_relay_route *route,uint32_t mode)
{
    if(route && route->endpoint){int rc=relay_link_ensure();if(rc<0)return rc;}
    SemaphoreHandle_t m=quic_lock();
    int rc=xip_quic_relay_set(&quic_table,I(ctx)->id,route,mode);
    xSemaphoreGive(m);
    if(!rc)xip_quic_classifier_enable(true);
    relay_link_gc();
    return rc;
}
static int c_relay_get(void *ctx,uint32_t index,xip_relay_route *route)
{
    SemaphoreHandle_t m=quic_lock();int rc=xip_quic_relay_get(&quic_table,I(ctx)->id,index,route);
    xSemaphoreGive(m);return rc;
}
static int c_relay_remove(void *ctx,const uint8_t *cid,size_t len)
{
    if(len>XIP_QUIC_CID_MAX)return XIP_ERR_ARG;
    SemaphoreHandle_t m=quic_lock();int rc=xip_quic_relay_remove(&quic_table,I(ctx)->id,cid,(unsigned)len);
    xSemaphoreGive(m);relay_link_gc();return rc;
}
static int c_relay_create(void *ctx,xip_relay_route *route,uint32_t idle_ms,uint32_t reuse)
{
    if(reuse>1)return XIP_ERR_ARG;
    if(route && route->endpoint){int rc=relay_link_ensure();if(rc<0)return rc;}
    SemaphoreHandle_t m=quic_lock();
    int rc=xip_quic_relay_create(&quic_table,I(ctx)->id,route,idle_ms,reuse!=0,(uint64_t)esp_timer_get_time());
    xSemaphoreGive(m);if(rc>=0)xip_quic_classifier_enable(true);relay_link_gc();return rc;
}
static int c_relay_stats(void *ctx,const uint8_t *cid,size_t len,xip_relay_stats *out)
{
    if(len>XIP_QUIC_CID_MAX)return XIP_ERR_ARG;
    SemaphoreHandle_t m=quic_lock();int rc=xip_quic_relay_stats(&quic_table,I(ctx)->id,cid,(unsigned)len,out);
    xSemaphoreGive(m);return rc;
}
static int c_relay_expire(void *ctx)
{
    SemaphoreHandle_t m=quic_lock();int rc=xip_quic_relay_expire(&quic_table,I(ctx)->id,(uint64_t)esp_timer_get_time());
    xSemaphoreGive(m);relay_link_gc();return rc;
}
static int c_relay_idle(void *ctx,const uint8_t *cid,size_t len,uint32_t idle_ms)
{
    if(len>XIP_QUIC_CID_MAX)return XIP_ERR_ARG;
    SemaphoreHandle_t m=quic_lock();int rc=xip_quic_relay_idle(&quic_table,I(ctx)->id,cid,(unsigned)len,idle_ms);
    xSemaphoreGive(m);return rc;
}
static const xip_quic quic_module_component={.version=2,.size=sizeof(xip_quic),.attach=c_quic_attach,.detach=c_quic_detach,.relay_set=c_relay_set,.relay_get=c_relay_get,.relay_remove=c_relay_remove,.relay_create=c_relay_create,.relay_stats=c_relay_stats,.relay_expire=c_relay_expire,.relay_idle=c_relay_idle};
static const xip_quic *quic_module_table(void){return &quic_module_component;}
