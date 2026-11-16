static void *memory_internal_alloc(size_t size);
/* Memory files share the runtime's serialized module owner. No background task. */
/* Packed best-effort log: 12-byte header, short name, body, 4-byte padding. */
struct rtc_file_record { uint32_t version; uint16_t len, checksum; uint8_t name_len, state; uint16_t span; };
_Static_assert(sizeof(struct rtc_file_record)==12, "RTC log header");
struct memory_node {
    struct memory_node *next, *old;
    uint8_t *data;
    uint32_t len, version;
    uint8_t ns, name[6], name_len, locked, pending, special, editing;
    int32_t rtc_at;
};
static struct memory_node *memory_files, *memory_writes;
static bool memory_files_initialized;
static bool memory_rtc_busy;
static uint8_t *rtc_log(void) { return retained_files.state.files; }
static size_t rtc_log_size(void) { return sizeof(retained_files.state.files); }
static struct rtc_file_record *rtc_record(size_t at) {
    if(at+sizeof(struct rtc_file_record)>rtc_log_size())return NULL;
    struct rtc_file_record *r=(void *)(rtc_log()+at);
    if(r->name_len<1 || r->name_len>6 || r->span!=((sizeof(*r)+r->name_len+r->len+3)&~3u) || at+r->span>rtc_log_size())return NULL;
    return r;
}
static uint16_t rtc_checksum(const struct rtc_file_record *r) {
    uint32_t h=r->version ^ ((uint32_t)r->len<<16) ^ r->name_len;
    const uint8_t *p=(const void *)(r+1);
    for(size_t i=0;i<(size_t)r->name_len+r->len;i++)h=(h^p[i])*16777619u;
    return (uint16_t)(h^(h>>16));
}
static bool rtc_valid(const struct rtc_file_record *r) {
    return r && (r->state==1 || r->state==2) && r->version && r->checksum==rtc_checksum(r);
}
static size_t rtc_end(void) {
    size_t at=4;struct rtc_file_record *r;
    while((r=rtc_record(at)) && rtc_valid(r))at+=r->span;
    return at;
}
static struct rtc_file_record *rtc_lookup(const uint8_t *name,size_t len) {
    struct rtc_file_record *found=NULL,*r;
    for(size_t at=4;(r=rtc_record(at)) && rtc_valid(r);at+=r->span)
        if(r->name_len==len && !memcmp(r+1,name,len))found=r;
    return found;
}
static void rtc_compact(void) {
    size_t dest=4;struct rtc_file_record *r;
    /* Only inspect the untouched suffix when deciding whether a record is live. */
    for(size_t at=4;(r=rtc_record(at)) && rtc_valid(r);) {
        size_t next=at+r->span;bool newer=false;struct rtc_file_record *q;
        for(size_t pos=next;(q=rtc_record(pos)) && rtc_valid(q);pos+=q->span)
            if(q->name_len==r->name_len && !memcmp(q+1,r+1,r->name_len)){newer=true;break;}
        if(r->state==1 && !newer){size_t span=r->span;memmove(rtc_log()+dest,r,span);dest+=span;}
        at=next;
    }
    /* An interrupted compaction may lose RTC files: this area is best effort. */
    memset(rtc_log()+dest,0,rtc_log_size()-dest);
}
static struct rtc_file_record *rtc_append(const uint8_t *name,size_t len,uint32_t size,uint32_t version) {
    size_t span=(sizeof(struct rtc_file_record)+len+size+3)&~3u;
    if(span>rtc_log_size()-4)return NULL;
    size_t at=rtc_end();
    if(at+span>rtc_log_size()){rtc_compact();at=rtc_end();}
    if(at+span>rtc_log_size())return NULL;
    memset(rtc_log()+at,0,span);
    struct rtc_file_record *r=(void *)(rtc_log()+at);
    r->version=version;r->len=size;r->name_len=len;r->span=span;memcpy(r+1,name,len);
    return r;
}
static void rtc_commit(struct rtc_file_record *r,uint8_t state) {
    r->checksum=rtc_checksum(r);__sync_synchronize();r->state=state;__sync_synchronize();
}
static bool memory_name(const uint8_t *name,size_t len) { return name && len && len<=6 && !memchr(name,0,len); }
static struct memory_node *memory_find(uint8_t ns,const uint8_t *name,size_t len) {
    for(struct memory_node *n=memory_files;n;n=n->next)
        if(n->ns==ns && n->name_len==len && !memcmp(n->name,name,len)) return n;
    return NULL;
}
static void memory_unlink(struct memory_node *n) {
    struct memory_node **p=&memory_files;
    while(*p && *p!=n)p=&(*p)->next;
    if(*p)*p=n->next;
}
static void memory_free(struct memory_node *n) {
    if(n->rtc_at<0) free(n->data);
    free(n);
}
static int memory_initialize(void) {
    if(memory_files_initialized)return 0;
    module_panics_init();
    uint32_t magic;memcpy(&magic,rtc_log(),4);
    if(esp_reset_reason()==ESP_RST_POWERON || magic!=0x52464c31u) {
        memset(rtc_log(),0,rtc_log_size());magic=0x52464c31u;memcpy(rtc_log(),&magic,4);
    }
    size_t end=rtc_end();memset(rtc_log()+end,0,rtc_log_size()-end);
    memory_files_initialized=true;return 0;
}
static void memory_describe(struct memory_node *n,xip_memory_file *out) {
    memset(out,0,sizeof(*out));out->data=n->data;out->len=n->len;out->version=n->version;
    out->token=(uintptr_t)n;memcpy(out->name,n->name,6);out->name_len=n->name_len;
    out->flags=(n->ns==4?1:3); /* readable=1, writable=2 */
}
static int memory_snapshot(struct memory_node *n) {
    size_t capacity=2048;
retry:
    n->data=malloc(capacity);if(!n->data)return XIP_ERR_NOMEM;
    int rc;
    if(n->ns==4 && n->name_len==6 && !memcmp(n->name,"status",6)) rc=xip_device_info(n->data,capacity);
    else if(n->ns==4 && n->name_len==5 && !memcmp(n->name,"panic",5)) {
        rc=module_panics.message_len?panic_record(n->data,capacity):0;
    } else if(n->ns==5 && n->name_len==4 && !memcmp(n->name,"quar",4)) {
        xip_cbor_w w;xip_cbor_init(&w,n->data,capacity);xip_cbor_array(&w,module_panics.count);
        for(uint32_t i=0;i<module_panics.count;i++)xip_cbor_text(&w,module_panics.names[i],strnlen(module_panics.names[i],16));
        rc=xip_cbor_done(&w);
    } else if(n->ns==5 && n->name_len==5 && !memcmp(n->name,"panic",5)) {
        rc=module_panics.message_len?panic_record(n->data,capacity):0;
    } else return XIP_ERR_NOTFOUND;
    if(rc==XIP_ERR_SIZE && capacity<16384){free(n->data);n->data=NULL;capacity*=2;goto retry;}
    if(rc<0)return rc;
    n->len=rc;return 0;
}
static int h_memory_open(void *user,uint8_t ns,const uint8_t *name,size_t len,uint8_t mode,uint32_t size,uint32_t version,xip_memory_file *out) {
    (void)user;
    if(!out || !memory_name(name,len) || ns<1 || ns>5 || mode>3)return XIP_ERR_ARG;
    int rc=memory_initialize();if(rc<0)return rc;
    for(struct memory_node *p=memory_writes;p;p=p->next)
        if(p->ns==ns && p->name_len==len && !memcmp(p->name,name,len))return XIP_ERR_BUSY;
    if(ns==3 && memory_rtc_busy)return XIP_ERR_BUSY;
    struct rtc_file_record *record=ns==3?rtc_lookup(name,len):NULL;
    uint32_t rtc_previous=record?record->version:0;
    if(record && record->state==2)record=NULL;
    struct memory_node *old=memory_find(ns,name,len);
    if(old && old->locked)return XIP_ERR_BUSY;
    if(mode==3 && (ns>2 || version))return XIP_ERR_ARG;
    if(ns<3 && mode!=1) {
        if(!old)return XIP_ERR_NOTFOUND;
        if(mode==3 && old->version==UINT32_MAX)return -21;
        old->locked=1;old->editing=mode==3;memory_describe(old,out);return 0;
    }
    if(ns==4 && mode==1)return XIP_ERR_UNSUPPORTED;
    if(ns==5) {
        bool quarantine=len==4 && !memcmp(name,"quar",4);
        bool panic=len==5 && !memcmp(name,"panic",5);
        if(!quarantine && !panic)return XIP_ERR_ARG;
        if(mode==1 && size>(quarantine?15u:1024u))return XIP_ERR_SIZE;
    }
    if(mode==1 && ns==3 && size>rtc_log_size()-4-sizeof(struct rtc_file_record)-len)return XIP_ERR_SIZE;
    uint32_t previous=ns==3?rtc_previous:(old?old->version:0);
    if(mode==1 && ns<4 && (previous==UINT32_MAX || (version && version<=previous)))return -21;
    /* Persistent user files preserve the reserve; bounded runtime snapshots may
     * still be read under pressure to diagnose the device. */
    struct memory_node *n=ns<3 && mode==1?memory_internal_alloc(sizeof(*n)):malloc(sizeof(*n));if(!n)return XIP_ERR_NOMEM;memset(n,0,sizeof(*n));
    n->rtc_at=-1;n->ns=ns;n->name_len=len;memcpy(n->name,name,len);n->special=ns>=4;
    if(ns==3 && mode!=1) {
        if(!record){free(n);return XIP_ERR_NOTFOUND;}
        n->rtc_at=(uint8_t *)record-rtc_log();n->data=(uint8_t *)(record+1)+len;
        n->len=record->len;n->version=record->version;n->special=1;memory_rtc_busy=true;
    } else if(mode==1) {
        if(ns==3) {
            record=rtc_append(name,len,size,version?version:previous+1);
            if(!record){free(n);return -20;}
            n->rtc_at=(uint8_t *)record-rtc_log();n->data=(uint8_t *)(record+1)+len;
            memory_rtc_busy=true;
        } else {
            n->data=ns==2?heap_caps_malloc(size?size:1,MALLOC_CAP_SPIRAM|MALLOC_CAP_8BIT):memory_internal_alloc(size?size:1);
            if(!n->data){free(n);return XIP_ERR_NOMEM;}
        }
        n->len=size;n->version=version?version:previous+1;n->pending=1;n->old=old;
        if(old)old->locked=1;
    } else {
        rc=memory_snapshot(n);if(rc<0){memory_free(n);return rc;}
    }
    if(n->pending){n->next=memory_writes;memory_writes=n;}
    memory_describe(n,out);return 0;
}
static int h_memory_close(void *user,uintptr_t token,bool commit) {
    (void)user;struct memory_node *n=(void *)token;if(!n)return XIP_ERR_ARG;
    if(n->ns==3) {
        if(n->pending){struct memory_node **p=&memory_writes;while(*p && *p!=n)p=&(*p)->next;if(*p)*p=n->next;}
        struct rtc_file_record *r=(void *)(rtc_log()+n->rtc_at);
        if(n->pending && commit)rtc_commit(r,1);
        else if(n->pending)memset(r,0,r->span);
        memory_rtc_busy=false;memory_free(n);return 0;
    }
    if(!n->pending && !n->special){
        if(n->editing){n->version++;n->editing=0;}
        n->locked=0;return 0;
    }
    if(n->pending){struct memory_node **p=&memory_writes;while(*p && *p!=n)p=&(*p)->next;if(*p)*p=n->next;}
    int rc=0;
    if(n->pending && commit) {
        if(n->ns==5 && n->name_len==4)rc=h_quarantine_clear(NULL,n->data,n->len);
        else if(n->ns==5) {
            /* Compare-and-drain: a later panic must never be cleared by an old collector. */
            if(module_panics.message_len) {
                int len=panic_record(module_panics.wire,992);
                if(len<0)rc=len;
                else if((uint32_t)len!=n->len || memcmp(module_panics.wire,n->data,n->len))rc=-21;
                else {module_panics.message_len=0;module_panics.logged=1;}
            }
        }
        else {
            if(rc==0) {
                if(n->old){struct memory_node *old=n->old;memory_unlink(old);memory_free(old);}
                n->old=NULL;n->pending=0;n->next=memory_files;memory_files=n;return 0;
            }
        }
    }
    if(n->old)n->old->locked=0;
    memory_free(n);return rc;
}
static int h_memory_remove(void *user,uint8_t ns,const uint8_t *name,size_t len) {
    (void)user;if(ns<1 || ns>3 || !memory_name(name,len))return XIP_ERR_ARG;
    int rc=memory_initialize();if(rc<0)return rc;
    if(ns==3) {
        if(memory_rtc_busy)return XIP_ERR_BUSY;
        struct rtc_file_record *old=rtc_lookup(name,len);
        if(!old || old->state==2)return XIP_ERR_NOTFOUND;
        uint32_t version=old->version;
        if(version==UINT32_MAX)return -21;
        struct rtc_file_record *r=rtc_append(name,len,0,version+1);
        if(!r)return -20;
        rtc_commit(r,2);return 0;
    }
    struct memory_node *n=memory_find(ns,name,len);if(!n)return XIP_ERR_NOTFOUND;
    if(n->locked)return XIP_ERR_BUSY;
    memory_unlink(n);
    memory_free(n);return 0;
}
static int h_memory_next(void *user,uint8_t ns,const uint8_t *after,size_t len,xip_memory_file *out) {
    (void)user;if(!out || ns<1 || ns>5 || len>6 || (len && !after))return XIP_ERR_ARG;
    int rc=memory_initialize();if(rc<0)return rc;
    uint8_t cursor[6]={0};if(len)memcpy(cursor,after,len);
    if(ns>=4) {
        const char *names[2]={"panic",ns==4?"status":"quar"};
        for(int i=0;i<2;i++) {
            uint8_t key[6]={0};size_t count=strlen(names[i]);memcpy(key,names[i],count);
            if(len && memcmp(key,cursor,6)<=0)continue;
            memset(out,0,sizeof(*out));memcpy(out->name,key,6);out->name_len=count;out->flags=ns==4?1:3;return 1;
        }
        return 0;
    }
    if(ns==3) {
        if(memory_rtc_busy)return XIP_ERR_BUSY;
        struct rtc_file_record *best=NULL,*r;uint8_t key[6],best_key[6];
        for(size_t at=4;(r=rtc_record(at)) && rtc_valid(r);at+=r->span) {
            if(r->state!=1 || rtc_lookup((const void *)(r+1),r->name_len)!=r)continue;
            memset(key,0,6);memcpy(key,r+1,r->name_len);
            if((len && memcmp(key,cursor,6)<=0) || (best && memcmp(key,best_key,6)>=0))continue;
            best=r;memcpy(best_key,key,6);
        }
        if(!best)return 0;
        memset(out,0,sizeof(*out));out->len=best->len;out->version=best->version;
        out->name_len=best->name_len;memcpy(out->name,best_key,6);out->flags=3;return 1;
    }
    struct memory_node *best=NULL;
    for(struct memory_node *n=memory_files;n;n=n->next) {
        if(n->ns!=ns || (len && memcmp(n->name,cursor,6)<=0))continue;
        if(!best || memcmp(n->name,best->name,6)<0)best=n;
    }
    if(!best)return 0;
    memory_describe(best,out);out->token=0;out->data=NULL;return 1;
}
static const xip_memory_files memory_files_service={1,sizeof(xip_memory_files),h_memory_open,h_memory_close,h_memory_next,h_memory_remove};
