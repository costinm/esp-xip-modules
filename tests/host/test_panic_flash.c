/* Actual one-shot panic writer, with only the SDK flash device replaced. */
#include <assert.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "xip.h"
#define ESP_OK 0
#define ESP_PARTITION_TYPE_APP 0
#define ESP_PARTITION_SUBTYPE_ANY 255
static atomic_uint flash_owner;
static struct {uint8_t wire[1024];} module_panics;
typedef struct {uint32_t address,size;} esp_partition_t;
static esp_partition_t partition={0x10000,65536};
static uint8_t flash[65536];
static int writes,fail_at;
static const esp_partition_t *esp_partition_find_first(int type,int subtype,const char *name){(void)type;(void)subtype;return !strcmp(name,"xip")?&partition:NULL;}
static int esp_flash_read(void *chip,void *out,uint32_t at,size_t len){(void)chip;if(at<partition.address || at-partition.address+len>sizeof(flash))return -1;memcpy(out,flash+at-partition.address,len);return 0;}
static int esp_flash_write(void *chip,const void *data,uint32_t at,size_t len){
 (void)chip;writes++;if(fail_at==writes)return -1;
 assert(at>=partition.address && at-partition.address+len<=sizeof(flash) && !(at%4) && !(len%4));
 const uint8_t *p=data;for(size_t i=0;i<len;i++){assert((flash[at-partition.address+i]&p[i])==p[i]);flash[at-partition.address+i]&=p[i];}return 0;
}
#include "../../components/xip_runtime/src/panic_flash.inc"
static uint32_t prepare(void){
 memset(flash,255,sizeof(flash));writes=0;fail_at=0;h_panic_flash_disarm(NULL);
 uint32_t at=0x11020;uint8_t *h=flash+at-partition.address-16;
 h[0]=0xb1;h[2]=0x0c;h[3]=4;memcpy(h+4,"panic\0\0\0",8);return at;
}
int main(void){
 uint32_t at=prepare(),base=0,len=0;
 assert(h_panic_flash_arm(NULL,at,1024)==0);h_panic_flash_range(NULL,&base,&len);assert(base==at && len==1024);
 memset(module_panics.wire,42,15);panic_flash_write(module_panics.wire,15);assert(writes==3);
 uint32_t header[4];memcpy(header,flash+at-partition.address,16);
 assert(header[0]==0x314c4650u && header[1]==15 && header[2]==panic_flash_checksum(module_panics.wire,15) && header[3]==0x50414e49u);
 assert(flash[at-partition.address+31]==255);assert(h_panic_flash_arm(NULL,at,1024)==XIP_ERR_BUSY);
 h_panic_flash_range(NULL,&base,&len);assert(!len);
 at=prepare();assert(h_panic_flash_arm(NULL,at,1024)==0);atomic_store(&flash_owner,1);
 panic_flash_write(module_panics.wire,15);assert(!writes);atomic_store(&flash_owner,0);
 at=prepare();assert(h_panic_flash_arm(NULL,at,1024)==0);fail_at=2;
 panic_flash_write(module_panics.wire,15);assert(writes==2);
 assert(flash[at-partition.address+12]==255); /* Failed record never commits. */
 at=prepare();flash[at-partition.address-12]='x';assert(h_panic_flash_arm(NULL,at,1024)==XIP_ERR_ARG);
 assert(h_panic_flash_arm(NULL,0x10,1024)==XIP_ERR_ARG);
 puts("panic flash: validated erased file, bounded one-shot NOR writes, commit-last and busy fallback ok");
}
