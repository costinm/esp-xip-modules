/* Owned endpoint packet seam. Driver callbacks touch only native completion
 * records; module code and its packet adapter run on the serialized worker. */
#define QUIC_MODULE_TX_MARKER (-2147483635)
#define QUIC_MODULE_COMPLETIONS 4
struct quic_module_done {
    unsigned state; /* 0 free, 1 physical driver owns slot, 2 returned by driver */
    int slot; uint32_t ticket,start,len,link; uint64_t peer;
    int outcome; uint32_t elapsed;
};
struct quic_module_state {
    uint32_t owner; int component,rx_method; uint32_t next_ticket;
    struct quic_module_done tx[QUIC_MODULE_COMPLETIONS];
};
static struct quic_module_state *quic_module;
static int c_quic_attach(void *ctx,int rx_method)
{
    struct xip_instance *in=I(ctx);
    if(rx_method<=0)return XIP_ERR_ARG;
    portENTER_CRITICAL(&registry_lock);
    bool exists=quic_module!=NULL;
    bool mine=exists && quic_module->owner==in->id;
    portEXIT_CRITICAL(&registry_lock);
    if(exists)return mine?XIP_OK:XIP_ERR_BUSY;
    struct quic_module_state *s=calloc(1,sizeof(*s));
    if(!s)return XIP_ERR_NOMEM;
    s->owner=in->id;s->component=in->component;s->rx_method=rx_method;
    portENTER_CRITICAL(&registry_lock);
    if(quic_module){portEXIT_CRITICAL(&registry_lock);free(s);return XIP_ERR_BUSY;}
    quic_module=s;atomic_store(&quic_module_owner,in->id);
    portEXIT_CRITICAL(&registry_lock);
    /* Endpoint activation is separate from an explicitly enabled native relay. */
    xip_quic_udp_ensure();
    return XIP_OK;
}
static void quic_module_release(struct xip_instance *in)
{
    portENTER_CRITICAL(&registry_lock);
    struct quic_module_state *s=quic_module;
    if(!s || s->owner!=in->id){portEXIT_CRITICAL(&registry_lock);return;}
    quic_module=NULL;atomic_store(&quic_module_owner,0);
    portEXIT_CRITICAL(&registry_lock);
    /* Completed leases are ours. Physical-driver leases are NOT reclaimed;
     * their old-epoch completion will release them entirely in native code. */
    for(unsigned i=0;i<QUIC_MODULE_COMPLETIONS;i++)if(s->tx[i].state==2)drop_slot(s->tx[i].slot);
    free(s);
}
static int c_quic_detach(void *ctx){quic_module_release(I(ctx));return XIP_OK;}
static int c_quic_pool_info(void *ctx,xip_quic_pool_info *out)
{
    struct xip_instance *in=I(ctx);xip_pool *p=pool_get();
    if(!out || out->size<sizeof(*out) || !p)return XIP_ERR_ARG;
    unsigned cap=xip_pool_slots(p);if(cap>in->held.quota)cap=in->held.quota;
    unsigned held=xip_held_count(&in->held,XIP_HELD_OWNED);
    unsigned available=xip_pool_available(p),room=held<cap?cap-held:0;
    if(available>room)available=room;
    *out=(xip_quic_pool_info){.size=sizeof(*out),.slot_bytes=xip_pool_slot_size(p),
        .headroom=XIP_PKT_HEADROOM,.max_packet=XIP_QUIC_MAX_PACKET,.capacity=cap,.available=available};
    return XIP_OK;
}
static int c_quic_acquire(void *ctx,uint32_t reserved,uint32_t *out)
{
    struct xip_instance *in=I(ctx);xip_pool *p=pool_get();
    if(!out || !p || reserved>UINT16_MAX)return XIP_ERR_ARG;
    if(xip_held_count(&in->held,XIP_HELD_OWNED)>=in->held.quota)return XIP_ERR_BUSY;
    int slot=xip_pool_acquire(p,(uint16_t)reserved);if(slot<0)return XIP_ERR_NOMEM;
    uint32_t h=xip_held_add(&in->held,slot,XIP_PKT_HEADROOM,0,XIP_HELD_OWNED);
    if(!h){drop_slot(slot);return XIP_ERR_BUSY;}*out=h;return XIP_OK;
}
static int c_quic_try_send(void *ctx,uint32_t packet,uint32_t start,uint32_t len,uint32_t link,uint64_t peer,uint32_t *ticket)
{
    struct xip_instance *in=I(ctx);xip_held_entry *held=xip_held_get(&in->held,packet);
    if(!held || held->state!=XIP_HELD_OWNED || !ticket || !len || len>XIP_QUIC_MAX_PACKET ||
       start>xip_pool_slot_size(pool) || len>xip_pool_slot_size(pool)-start)return XIP_ERR_ARG;
    int at=-1;uint32_t txid=0;
    portENTER_CRITICAL(&registry_lock);
    if(quic_module && quic_module->owner==in->id){
        for(unsigned i=0;i<QUIC_MODULE_COMPLETIONS;i++)if(!quic_module->tx[i].state){at=(int)i;break;}
        if(at>=0){
            txid=++quic_module->next_ticket;if(!txid)txid=++quic_module->next_ticket;
            quic_module->tx[at]=(struct quic_module_done){.state=1,.slot=held->slot,.ticket=txid,
                .start=start,.len=len,.link=link,.peer=peer};
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if(at<0)return XIP_ERR_BUSY;
    /* link_tx borrows the one moved slot and retains it on EVERY refusal.
     * Its native completion is pre-reserved, so inbox pressure cannot lose it. */
    xip_held_entry moved=xip_held_take(&in->held,packet);
    int rc=link_tx(link,moved.slot,(uint16_t)start,(uint16_t)len,peer,(int)in->id,QUIC_MODULE_TX_MARKER,(int)txid);
    if(rc<0){
        *held=moved; /* same handle generation and exact prepared bytes */
        portENTER_CRITICAL(&registry_lock);
        if(quic_module && quic_module->owner==in->id)quic_module->tx[at].state=0;
        portEXIT_CRITICAL(&registry_lock);
        return rc;
    }
    *ticket=txid;in->stats.pkt_sent++;return XIP_OK;
}
static bool quic_module_complete(int slot,int sender,int component,int method,int outcome,uint32_t elapsed)
{
    if(component!=QUIC_MODULE_TX_MARKER)return false;
    bool retained=false;
    portENTER_CRITICAL(&registry_lock);
    if(quic_module && quic_module->owner==(uint32_t)sender){
        for(unsigned i=0;i<QUIC_MODULE_COMPLETIONS;i++){
            struct quic_module_done *d=&quic_module->tx[i];
            if(d->state==1 && d->slot==slot && d->ticket==(uint32_t)method){
                d->outcome=outcome;d->elapsed=elapsed;d->state=2;retained=true;break;
            }
        }
    }
    portEXIT_CRITICAL(&registry_lock);
    if(!retained)drop_slot(slot);
    return true;
}
static int c_quic_poll_tx(void *ctx,xip_quic_tx_completion *out)
{
    struct xip_instance *in=I(ctx);
    if(!out || out->size<sizeof(*out))return XIP_ERR_ARG;
    int rc=0;
    portENTER_CRITICAL(&registry_lock);
    if(!quic_module || quic_module->owner!=in->id){portEXIT_CRITICAL(&registry_lock);return XIP_ERR_NOTFOUND;}
    for(unsigned i=0;i<QUIC_MODULE_COMPLETIONS;i++){
        struct quic_module_done *d=&quic_module->tx[i];if(d->state!=2)continue;
        uint32_t h=xip_held_add(&in->held,d->slot,(uint16_t)d->start,(uint16_t)d->len,XIP_HELD_OWNED);
        if(!h){rc=XIP_ERR_BUSY;break;}
        *out=(xip_quic_tx_completion){.size=sizeof(*out),.packet=h,.ticket=d->ticket,.start=d->start,
            .len=d->len,.link=d->link,.peer=d->peer,.outcome=d->outcome,.elapsed_us=d->elapsed};
        d->state=0;rc=1;break;
    }
    portEXIT_CRITICAL(&registry_lock);return rc;
}
static bool quic_module_offer(const xip_rx_info *info,int slot,uint16_t start,uint16_t len)
{
    uint32_t owner=0;int component=0,method=0;
    portENTER_CRITICAL(&registry_lock);
    if(quic_module){owner=quic_module->owner;component=quic_module->component;method=quic_module->rx_method;}
    portEXIT_CRITICAL(&registry_lock);
    if(!owner)return false;
    struct xip_instance *in=find(owner);
    if(deliver_checked(in,owner,slot,start,len,component,method,(uint32_t)info->link,info->source,
                       info->signal,XIP_CALL_LINK,info->time_us))drop_slot(slot);
    return true;
}
static const xip_quic quic_module_component={.version=1,.size=sizeof(xip_quic),.attach=c_quic_attach,
    .detach=c_quic_detach,.pool_info=c_quic_pool_info,.acquire=c_quic_acquire,.try_send=c_quic_try_send,.poll_tx=c_quic_poll_tx};
static const xip_quic *quic_module_table(void){return &quic_module_component;}
