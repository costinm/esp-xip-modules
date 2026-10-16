/* One temporary task per accepted operation, released after completion. */
typedef struct {
    atomic_bool ready; uint32_t owner, address, len; int slot; const uint8_t *data;
    int32_t component, method; uint64_t id;
} flash_job;
static atomic_uint flash_owner;
static atomic_bool flash_cancelled;
static int flash_range(uint32_t base, uint32_t off, size_t len, uint32_t *address)
{
    uint32_t size;
    if (!len || len > UINT32_MAX || base > UINT32_MAX-off || esp_flash_get_size(NULL, &size) != ESP_OK) return XIP_ERR_ARG;
    *address = base+off;
    return *address <= size && len <= size-*address ? 0 : XIP_ERR_ARG;
}
static void flash_task(void *arg)
{
    flash_job *j = arg;
    while (!atomic_load(&j->ready)) vTaskDelay(1);
    int rc = XIP_ERR_IO;
    if (!atomic_load(&flash_cancelled)) {
        rc = (j->data ? esp_flash_write(NULL, j->data, j->address, j->len)
                      : esp_flash_erase_region(NULL, j->address, j->len)) == ESP_OK ? 0 : XIP_ERR_IO;
    }
    if (j->slot >= 0) drop_slot(j->slot);
    xip_event_t ev = {.kind=XIP_EV_IO, .inst=j->owner, .component=j->component, .method=j->method, .value=rc, .id=j->id};
    free(j);
    /* The worker can preempt this lower-priority task as soon as the event is
     * queued. Publish the idle state before it can dispatch the completion.
     * Its instance lookup uses registry_lock, also on multicore hosts. */
    for (;;) {
        portENTER_CRITICAL(&registry_lock);
        bool done = atomic_load(&flash_cancelled) || xQueueSend(worker_queue, &ev, 0) == pdTRUE;
        if (done) atomic_store(&flash_owner, 0);
        portEXIT_CRITICAL(&registry_lock);
        if (done) break;
        vTaskDelay(1);
    }
    vTaskDelete(NULL);
}
static int flash_submit(void *user, uint32_t address, uint32_t len, uint32_t pkt, const uint8_t *data,
                        int32_t component, int32_t method, uint64_t id)
{
    uint32_t expected = 0;
    if (!atomic_compare_exchange_strong(&flash_owner, &expected, I(user)->id)) return XIP_ERR_BUSY;
    atomic_store(&flash_cancelled, false);
    flash_job *j = malloc(sizeof(*j));
    if (!j) { atomic_store(&flash_owner, 0); return XIP_ERR_NOMEM; }
    *j=(flash_job){.owner=I(user)->id,.address=address,.len=len,.slot=-1,.data=data,.component=component,.method=method,.id=id};
    if (pkt) j->slot=xip_held_get(&I(user)->held,pkt)->slot;
    if (xTaskCreate(flash_task,"xip_flash",4096,j,CONFIG_XIP_WORKER_PRIORITY-1,NULL)!=pdPASS) {
        free(j); atomic_store(&flash_owner,0); return XIP_ERR_NOMEM;
    }
    if (pkt) (void)xip_held_take(&I(user)->held,pkt);
    atomic_store(&j->ready,true);
    return 0;
}
static int h_flash_erase(void *u,uint32_t base,uint32_t off,uint32_t len,int32_t c,int32_t m,uint64_t id)
{
    uint32_t addr; int rc=flash_range(base,off,len,&addr);
    if (rc || addr%4096 || len%4096) return XIP_ERR_ARG;
    return flash_submit(u,addr,len,0,NULL,c,m,id);
}
static int h_flash_write(void *u,uint32_t base,uint32_t off,uint32_t pkt,uint32_t start,uint32_t len,int32_t c,int32_t m,uint64_t id)
{
    uint32_t addr; uint8_t *buf; size_t cap;
    int rc=flash_range(base,off,len,&addr); if(rc) return rc;
    rc=h_pkt_span(u,pkt,&buf,&cap); if(rc) return rc;
    if(start>cap || len>cap-start) return XIP_ERR_ARG;
    return flash_submit(u,addr,len,pkt,buf+start,c,m,id);
}
static int h_flash_read(void *u,uint32_t base,uint32_t off,uint8_t *out,size_t len)
{
    (void)u; uint32_t addr;
    if (!out || len>4096 || flash_range(base,off,len,&addr)) return XIP_ERR_ARG;
    return esp_flash_read(NULL,out,addr,len)==ESP_OK ? (int)len : XIP_ERR_IO;
}
static void flash_release(uint32_t owner)
{
    if (atomic_load(&flash_owner)!=owner) return;
    atomic_store(&flash_cancelled,true);
    while(atomic_load(&flash_owner)==owner) vTaskDelay(1);
}
