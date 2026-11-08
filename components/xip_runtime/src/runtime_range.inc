/* Locate the executing image through its mapped app description, rather than
 * treating the entire XIP partition as executing code. No flash is modified. */
static bool running_image_range(uint32_t partition_base, uint32_t partition_size,
                                uint32_t *base, uint32_t *span)
{
    size_t desc=spi_flash_cache2phys(esp_app_get_description());
    if(desc==SPI_FLASH_CACHE2PHYS_FAIL || desc<32 || desc>UINT32_MAX)return false;
    uint32_t start=(uint32_t)desc-32;
    if(start%65536 || start<partition_base || start-partition_base>=partition_size)return false;
    uint32_t room=partition_size-(start-partition_base);
    if(room>255u*65536u)room=255u*65536u;
    uint8_t h[24];uint32_t magic;
    if(room<36 || esp_flash_read(NULL,h,start,sizeof h)!=ESP_OK || h[0]!=0xe9 || !h[1] || h[1]>16 ||
       esp_flash_read(NULL,&magic,(uint32_t)desc,sizeof magic)!=ESP_OK || magic!=0xabcd5432)return false;
    uint32_t at=24;
    for(unsigned i=0;i<h[1];i++){
        uint32_t segment[2];
        if(at>room-8 || esp_flash_read(NULL,segment,start+at,sizeof segment)!=ESP_OK)return false;
        at+=8;
        if(segment[1]>room-at)return false;
        at+=segment[1];
    }
    uint32_t length=(at|15u)+1+(h[23]?32u:0u);
    if(length>room)return false;
    *base=start;*span=(length+65535u)&~65535u;
    return *span<=partition_size-(start-partition_base);
}
