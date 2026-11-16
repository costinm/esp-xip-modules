/* RTC survives software restart; quarantine is deliberately cleared on power-on.
 * No allocation, module callback or filesystem operation runs on the panic path. */
struct module_panic_state {
    uint32_t magic, count;
    char names[32][16];
    char last_module[16];
    uint32_t message_len;
    uint8_t message[768];
    char alloc_module[16];
    uint32_t alloc_size, free, largest, used, quota, denial;
    union {
        uint8_t wire[1024];
        struct { uint8_t scratch[992]; uint32_t pressure[8]; };
    };
    uint32_t logged, module_version;
};
#if (defined(CONFIG_ESP32_RTCDATA_IN_FAST_MEM) && CONFIG_ESP32_RTCDATA_IN_FAST_MEM) || (defined(CONFIG_ESP32S3_RTCDATA_IN_FAST_MEM) && CONFIG_ESP32S3_RTCDATA_IN_FAST_MEM)
#error "Retained filesystem requires RTC data in SLOW memory"
#endif
/* One linker-owned reservation, including panic state, excluded from SDK heap. */
static RTC_NOINIT_ATTR union {
    struct { struct module_panic_state panic; uint8_t files[6144 - sizeof(struct module_panic_state)]; } state;
    uint8_t bytes[6144];
} retained_files;
_Static_assert(sizeof(retained_files) == 6144, "RTC filesystem reservation");
#define module_panics retained_files.state.panic
/* Fleet v2 had a shorter message and scratch buffer; its prefix is unchanged. */
struct module_panic_v2 {
    uint32_t magic,count;char names[32][16],last_module[16];uint32_t message_len;
    uint8_t message[192];char alloc_module[16];
    uint32_t alloc_size,free,largest,used,quota,denial;uint8_t wire[320];
};
_Static_assert(sizeof(struct module_panic_v2)==1092, "RTC panic v2 layout");
_Static_assert(sizeof(((struct module_panic_state *)0)->message)+128 <= sizeof(((struct module_panic_state *)0)->scratch), "panic encoder scratch and pressure tail");
static bool module_panics_checked;
static void module_panics_init(void)
{
    if (module_panics_checked) return;
    if (module_panics.magic==0x50414e32u && module_panics.count<=32 && module_panics.message_len<=192) {
        const uint8_t *old=(const void *)&module_panics;
        /* Move allocation metadata before clearing the expanded message range. */
        memmove(module_panics.alloc_module,old+__builtin_offsetof(struct module_panic_v2,alloc_module),16);
        memmove(&module_panics.alloc_size,old+__builtin_offsetof(struct module_panic_v2,alloc_size),6*sizeof(uint32_t));
        memset(module_panics.message+192,0,sizeof(module_panics.message)-192);
        module_panics.magic=0x50414e34u;
        module_panics.logged=module_panics.message_len?0:1;module_panics.module_version=0;
    }
    if (module_panics.magic == 0x50414e33u && module_panics.count <= 32 && module_panics.message_len <= sizeof(module_panics.message)) {
        module_panics.magic=0x50414e34u; module_panics.logged=module_panics.message_len?0:1;module_panics.module_version=0;
    }
    if (module_panics.magic == 0x50414e34u && module_panics.count <= 32 && module_panics.message_len <= sizeof(module_panics.message)) {
        /* v5 uses only the old scratch tail; RTC files keep identical offsets. */
        memset(module_panics.pressure,0,sizeof(module_panics.pressure));
        module_panics.magic=0x50414e35u;
    }
    if (esp_reset_reason() == ESP_RST_POWERON || module_panics.magic != 0x50414e35u || module_panics.count > 32 || module_panics.message_len > sizeof(module_panics.message)) {
        memset(&module_panics, 0, sizeof(module_panics));
        module_panics.magic = 0x50414e35u;
    }
    module_panics_checked = true;
}
bool xip_module_panicked(const char *name)
{
    module_panics_init();
    for (uint32_t i=0; i<module_panics.count; i++)
        if (!strncmp(name, module_panics.names[i], 16)) return true;
    /* A full quarantine fails closed instead of starting an unrecordable module. */
    return module_panics.count == 32;
}
/* Live failure context is separate from the retained panic snapshot. New allocations
 * after reboot must not overwrite the pending diagnostic before flash commits it. */
static struct {
    char module[16];
    uint32_t size, free, largest, used, quota, denial;
    uint32_t pressure[8];
    int64_t at;
} allocation_failure;
static uint32_t allocation_reclaimed_bytes;
static void module_allocation_failed(struct xip_instance *in, size_t size, unsigned reason)
{
    module_panics_init();
    allocation_failure.at=esp_timer_get_time();
    strlcpy(allocation_failure.module, in->name, 16);
    allocation_failure.size=size;
    allocation_failure.free=heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
    allocation_failure.largest=heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT);
    allocation_failure.used=in->mem.used; allocation_failure.quota=in->mem.quota; allocation_failure.denial=reason;
    allocation_pressure_snapshot(allocation_failure.pressure);
    allocation_failure.pressure[7]=allocation_reclaimed_bytes;
    xip_cbor_w w; xip_cbor_init(&w,module_panics.wire,sizeof(module_panics.scratch));
    xip_cbor_map(&w,16);
    xip_cbor_uint(&w,0);xip_cbor_uint(&w,4);
    xip_cbor_uint(&w,1);xip_cbor_text(&w,in->name,strnlen(in->name,16));
    xip_cbor_uint(&w,2);xip_cbor_uint(&w,size);
    xip_cbor_uint(&w,3);xip_cbor_uint(&w,allocation_failure.free);
    xip_cbor_uint(&w,4);xip_cbor_uint(&w,allocation_failure.largest);
    xip_cbor_uint(&w,5);xip_cbor_uint(&w,allocation_failure.used);
    xip_cbor_uint(&w,6);xip_cbor_uint(&w,allocation_failure.quota);
    xip_cbor_uint(&w,7);xip_cbor_uint(&w,reason);
    for(unsigned k=0;k<8;k++){xip_cbor_uint(&w,8+k);xip_cbor_uint(&w,allocation_failure.pressure[k]);}
    int n=xip_cbor_done(&w);if(n>0)(void)xip_serial_write_frame(1,module_panics.wire,(size_t)n);
}
static int panic_record(uint8_t *out,size_t capacity)
{
    bool allocation=!strncmp(module_panics.last_module,module_panics.alloc_module,16) && module_panics.alloc_size;
    size_t len=module_panics.message_len;

    xip_cbor_w w; xip_cbor_init(&w,out,capacity);
    xip_cbor_map(&w,allocation?19:5);
    xip_cbor_uint(&w,0);xip_cbor_uint(&w,3);
    xip_cbor_uint(&w,1);xip_cbor_text(&w,"panic",5);
    xip_cbor_uint(&w,2);xip_cbor_text(&w,module_panics.last_module,strnlen(module_panics.last_module,16));
    xip_cbor_uint(&w,3);xip_cbor_bytes(&w,module_panics.message,len);
    if(allocation){
        xip_cbor_uint(&w,4);xip_cbor_uint(&w,module_panics.alloc_size);
        xip_cbor_uint(&w,5);xip_cbor_uint(&w,module_panics.free);
        xip_cbor_uint(&w,6);xip_cbor_uint(&w,module_panics.largest);
        xip_cbor_uint(&w,7);xip_cbor_uint(&w,module_panics.used);
        xip_cbor_uint(&w,8);xip_cbor_uint(&w,module_panics.quota);
        xip_cbor_uint(&w,9);xip_cbor_uint(&w,module_panics.denial);
    }
    xip_cbor_uint(&w,10);xip_cbor_uint(&w,module_panics.module_version);
    if(allocation)for(unsigned k=0;k<8;k++){xip_cbor_uint(&w,11+k);xip_cbor_uint(&w,module_panics.pressure[k]);}
    return xip_cbor_done(&w);
}
static void h_module_panic(void *user, const uint8_t *message, size_t len)
{
    struct xip_instance *in = I(user);
    module_panics_init();
    if (!xip_module_panicked(in->name) && module_panics.count < 32) {
        strlcpy(module_panics.names[module_panics.count], in->name, 16);
        module_panics.count++;
    }
    module_panics.alloc_size=0;
    if (!strncmp(in->name,allocation_failure.module,16) && allocation_failure.size &&
        esp_timer_get_time()-allocation_failure.at<=1000000) {
        strlcpy(module_panics.alloc_module,in->name,16);
        module_panics.alloc_size=allocation_failure.size;
        module_panics.free=allocation_failure.free;module_panics.largest=allocation_failure.largest;
        module_panics.used=allocation_failure.used;module_panics.quota=allocation_failure.quota;
        module_panics.denial=allocation_failure.denial;
        memcpy(module_panics.pressure,allocation_failure.pressure,sizeof(module_panics.pressure));
    }
    strlcpy(module_panics.last_module, in->name, 16);
    len = len > sizeof(module_panics.message) ? sizeof(module_panics.message) : len;
    if (len) memcpy(module_panics.message, message, len);
    module_panics.message_len = len;
    module_panics.logged = 0;
    module_panics.module_version = in->version;
    int n=panic_record(module_panics.wire,sizeof(module_panics.scratch));
    if(n>0)(void)xip_serial_write_frame(1,module_panics.wire,(size_t)n);
    if(n>0)panic_flash_write(module_panics.wire,(size_t)n);
    esp_restart();
}
static int h_panic_read(void *user, uint8_t *out, size_t capacity, uint32_t *address)
{
    module_panics_init();
    if (!module_panics.message_len || module_panics.logged) return 0;
    *address = (uint32_t)I(user)->component;
    return panic_record(out,capacity);
}
static void h_panic_logged(void *user) { (void)user; module_panics.logged = 1; }
/* get_component("module-panic"), shared with xip_abi::heap::module_panic. */
static const xip_module_panic module_panic_service = {1, sizeof(xip_module_panic), h_module_panic, h_panic_read, h_panic_logged};


static int h_quarantine_clear(void *user, const uint8_t *name, size_t len)
{
    (void)user;
    if (!name || !len || len >= sizeof(module_panics.names[0]) || memchr(name, 0, len))
        return XIP_ERR_ARG;
    module_panics_init();
    for (uint32_t i = 0; i < module_panics.count; i++) {
        if (strlen(module_panics.names[i]) != len || memcmp(name, module_panics.names[i], len)) continue;
        memmove(module_panics.names[i], (const uint8_t *)module_panics.names + (i+1) * sizeof(module_panics.names[0]),
                (module_panics.count-i-1) * sizeof(module_panics.names[0]));
        memset(module_panics.names[--module_panics.count], 0, sizeof(module_panics.names[0]));
        break;
    }
    return 0;
}
static const xip_module_quarantine module_quarantine_service = {
    1, sizeof(xip_module_quarantine), h_quarantine_clear
};
