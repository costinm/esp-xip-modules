/* Production runtime range reader, with a whole-pool SDK partition and mapped A/B descriptions. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#define ESP_OK 0
#define SPI_FLASH_CACHE2PHYS_FAIL UINT32_MAX
static uint8_t flash[4*1024*1024];
static size_t mapped;
static const void *esp_app_get_description(void){return &mapped;}
static size_t spi_flash_cache2phys(const void *p){(void)p;return mapped;}
static int esp_flash_read(void *chip,void *out,uint32_t at,size_t n){
    (void)chip;if(at>sizeof flash || n>sizeof flash-at)return -1;memcpy(out,flash+at,n);return 0;
}
#include "../../components/xip_runtime/src/runtime_range.inc"
static void put(uint32_t at,uint32_t v){memcpy(flash+at,&v,4);}
static void image(uint32_t at,uint32_t body){
    flash[at]=0xe9;flash[at+1]=1;flash[at+23]=1;
    put(at+28,body);put(at+32,0xabcd5432);mapped=at+32;
}
int main(void){
    memset(flash,255,sizeof flash);uint32_t base=0,span=0;
    image(0x10000,0x170001);
    assert(running_image_range(0x10000,0x3f0000,&base,&span));
    assert(base==0x10000 && span==0x180000);
    image(0x10000,0x200000);
    assert(running_image_range(0x10000,0x3f0000,&base,&span));
    assert(base==0x10000 && span==0x210000);
    image(0x280000,0x160010);
    assert(running_image_range(0x10000,0x3f0000,&base,&span));
    assert(base==0x280000 && span==0x170000);
    /* The actual mapped B wins even when A is valid and has a higher version. */
    put(0x280000+28,0x200000);
    assert(!running_image_range(0x10000,0x3f0000,&base,&span));
    mapped=SPI_FLASH_CACHE2PHYS_FAIL;assert(!running_image_range(0x10000,0x3f0000,&base,&span));
    mapped=0x280021;assert(!running_image_range(0x10000,0x3f0000,&base,&span));
    image(0x280000,0x1000);put(0x280020,0);
    assert(!running_image_range(0x10000,0x3f0000,&base,&span));
    puts("running image range: actual mapped A/B, pool partition and conservative malformed fallback ok");
}
