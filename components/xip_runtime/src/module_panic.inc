/* RTC survives software restart; quarantine is deliberately cleared on power-on.
 * No allocation, module callback or filesystem operation runs on the panic path. */
static RTC_NOINIT_ATTR struct {
    uint32_t magic, count;
    char names[32][16];
    char last_module[16];
    uint32_t message_len;
    uint8_t message[192];
    char alloc_module[16];
    uint32_t alloc_size, free, largest, used, quota, denial;
    uint8_t wire[320];
} module_panics;
static bool module_panics_checked;
static void module_panics_init(void)
{
    if (module_panics_checked) return;
    if (esp_reset_reason() == ESP_RST_POWERON || module_panics.magic != 0x50414e32u || module_panics.count > 32 || module_panics.message_len > 192) {
        memset(&module_panics, 0, sizeof(module_panics));
        module_panics.magic = 0x50414e32u;
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
    int64_t at;
} allocation_failure;
static void module_allocation_failed(struct xip_instance *in, size_t size, unsigned reason)
{
    module_panics_init();
    allocation_failure.at=esp_timer_get_time();
    strlcpy(allocation_failure.module, in->name, 16);
    allocation_failure.size=size;
    allocation_failure.free=heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
    allocation_failure.largest=heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT);
    allocation_failure.used=in->mem.used; allocation_failure.quota=in->mem.quota; allocation_failure.denial=reason;
    xip_cbor_w w; xip_cbor_init(&w,module_panics.wire,sizeof(module_panics.wire));
    xip_cbor_map(&w,8);
    xip_cbor_uint(&w,0);xip_cbor_uint(&w,4);
    xip_cbor_uint(&w,1);xip_cbor_text(&w,in->name,strnlen(in->name,16));
    xip_cbor_uint(&w,2);xip_cbor_uint(&w,size);
    xip_cbor_uint(&w,3);xip_cbor_uint(&w,allocation_failure.free);
    xip_cbor_uint(&w,4);xip_cbor_uint(&w,allocation_failure.largest);
    xip_cbor_uint(&w,5);xip_cbor_uint(&w,allocation_failure.used);
    xip_cbor_uint(&w,6);xip_cbor_uint(&w,allocation_failure.quota);
    xip_cbor_uint(&w,7);xip_cbor_uint(&w,reason);
    int n=xip_cbor_done(&w);if(n>0)(void)xip_serial_write_frame(1,module_panics.wire,(size_t)n);
}
static int panic_record(uint8_t *out,size_t capacity)
{
    bool allocation=!strncmp(module_panics.last_module,module_panics.alloc_module,16) && module_panics.alloc_size;
    size_t len=module_panics.message_len;
    if(allocation && len>128)len=128;
    xip_cbor_w w; xip_cbor_init(&w,out,capacity);
    xip_cbor_map(&w,allocation?10:4);
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
    }
    strlcpy(module_panics.last_module, in->name, 16);
    len = len > sizeof(module_panics.message) ? sizeof(module_panics.message) : len;
    if (len) memcpy(module_panics.message, message, len);
    module_panics.message_len = len;
    int n=panic_record(module_panics.wire,sizeof(module_panics.wire));
    if(n>0)(void)xip_serial_write_frame(1,module_panics.wire,(size_t)n);
    esp_restart();
}
static int h_panic_read(void *user, uint8_t *out, size_t capacity, uint32_t *address)
{
    module_panics_init();
    if (!module_panics.message_len) return 0;
    *address = (uint32_t)I(user)->component;
    return panic_record(out,capacity);
}
static void h_panic_logged(void *user) { (void)user; module_panics.message_len = 0; }
/* get_component("module-panic"), shared with xip_abi::heap::module_panic. */
static const xip_module_panic module_panic_service = {1, sizeof(xip_module_panic), h_module_panic, h_panic_read, h_panic_logged};

