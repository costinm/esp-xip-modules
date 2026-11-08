/* Regression: allocation/output under the allocator lock caused a real WDT. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include "xip_cbor.h"
#define XIP_PORT_CONSOLE 1
static uint32_t entry_heap_total=348440, entry_heap_free=320000;
static bool locked;
static unsigned packets;
static int xip_serial_write_frame(uint32_t port,const void *data,size_t len){assert(!locked);assert(port==1);assert(data&&len>0);packets++;return 0;}
#define MALLOC_CAP_8BIT 4
#define XIP_OK 0
#define XIP_ERR_NOMEM -1
#define XIP_ERR_SIZE -2
typedef struct {intptr_t start,end;} walker_heap_into_t;
typedef struct {void *ptr;size_t size;bool used;} walker_block_info_t;
static bool oom;
static unsigned block_count, allocations, frees, printed;
static void *heap_caps_malloc(size_t size,int caps){assert(!locked);assert(caps==4);if(oom)return NULL;allocations++;return malloc(size);}
static void heap_caps_free(void *ptr){assert(!locked);frees++;free(ptr);}
static void heap_caps_walk(int caps,bool (*cb)(walker_heap_into_t,walker_block_info_t,void*),void *ctx){assert(caps==4);locked=true;for(unsigned i=0;i<block_count;i++)assert(cb((walker_heap_into_t){0x40800000,0x40880000},(walker_block_info_t){(void*)(uintptr_t)(0x40800000+16*i),16,true},ctx));locked=false;}
static int checked_printf(const char *format,...){(void)format;assert(!locked);printed++;return 0;}
#define printf checked_printf
#include "heap_snapshot.inc"
int main(void){
 block_count=500;assert(heap_dump_blocks(false)==XIP_OK);assert(printed==501);assert(allocations==1&&frees==1);
 printed=0;block_count=1200;assert(heap_dump_blocks(false)==XIP_ERR_SIZE);assert(printed==1025);assert(allocations==2&&frees==2);
 printed=0;block_count=500;assert(heap_dump_blocks(true)==XIP_OK);assert(packets==502);assert(printed==0);
 oom=true;assert(heap_dump_blocks(false)==XIP_ERR_NOMEM);assert(allocations==frees);
}
