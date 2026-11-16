/* Rust module panic path only: RTOS/SDK flash driver are still running. No erase,
 * allocation, filesystem dispatch or retry; a busy flash job falls back to RTC. */
static uint32_t panic_flash_address, panic_flash_length;
static uint32_t panic_flash_checksum(const uint8_t *p,size_t len) {
    uint32_t h=2166136261u;for(size_t i=0;i<len;i++)h=(h^p[i])*16777619u;return h;
}
static int h_panic_flash_arm(void *user,uint32_t address,uint32_t len) {
    (void)user;
    if(panic_flash_address || atomic_load(&flash_owner))return XIP_ERR_BUSY;
    const esp_partition_t *part=esp_partition_find_first(ESP_PARTITION_TYPE_APP,ESP_PARTITION_SUBTYPE_ANY,"xip");
    if(!part || address%4 || len<1024 || len>8192 || address<part->address+16 ||
       address>part->address+part->size || len>part->address+part->size-address)return XIP_ERR_ARG;
    uint8_t header[16];
    if(esp_flash_read(NULL,header,address-16,16)!=ESP_OK || header[0]!=0xb1 ||
       (uint32_t)(header[2]|header[3]<<8)!=len+12 || memcmp(header+4,"panic\0\0\0",8))return XIP_ERR_ARG;
    uint8_t bytes[64];
    for(uint32_t at=0;at<len;at+=sizeof(bytes)) {
        size_t n=len-at<sizeof(bytes)?len-at:sizeof(bytes);
        if(esp_flash_read(NULL,bytes,address+at,n)!=ESP_OK)return XIP_ERR_IO;
        for(size_t i=0;i<n;i++)if(bytes[i]!=255)return XIP_ERR_BUSY;
    }
    panic_flash_length=len;panic_flash_address=address;return 0;
}
static void h_panic_flash_range(void *user,uint32_t *address,uint32_t *len) {
    (void)user;*address=panic_flash_address;*len=panic_flash_address?panic_flash_length:0;
}
static void h_panic_flash_disarm(void *user) {(void)user;panic_flash_address=0;panic_flash_length=0;}
static void panic_flash_write(const uint8_t *data,size_t len) {
    uint32_t address=panic_flash_address,capacity=panic_flash_length;
    panic_flash_address=0; /* One shot, including failed/partial writes. */
    if(!address || atomic_load(&flash_owner) || len>sizeof(module_panics.wire) ||
       16+((len+3)&~3u)>capacity)return;
    uint32_t header[4]={0x314c4650u,(uint32_t)len,panic_flash_checksum(data,len),0x50414e49u};
    /* wire is aligned RTC storage; pad its unused tail for the native writer. */
    size_t padded=(len+3)&~3u;
    if(padded>len)memset(module_panics.wire+len,255,padded-len);
    if(esp_flash_write(NULL,data,address+16,padded)!=ESP_OK)return;
    if(esp_flash_write(NULL,header,address,12)!=ESP_OK)return;
    (void)esp_flash_write(NULL,&header[3],address+12,4); /* Commit last. */
}
static const xip_panic_flash panic_flash_service={1,sizeof(xip_panic_flash),h_panic_flash_arm,h_panic_flash_range,h_panic_flash_disarm};
