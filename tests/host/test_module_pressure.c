/* Exercise production reserve admission/reclaim while replacing SDK heap totals. */
#include <assert.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#define CONFIG_XIP_PKT_HEAP_RESERVE 16384
#define MALLOC_CAP_DEFAULT 0
#define MALLOC_CAP_INTERNAL 4
#define MALLOC_CAP_8BIT 2
static void *heap_caps_malloc(size_t size,int caps){assert(caps==(MALLOC_CAP_INTERNAL|MALLOC_CAP_8BIT));return malloc(size);}
struct memory {uint32_t used,quota;};
struct instance {struct memory mem;};
static struct instance *I(void *user){return user;}
static size_t available,cached,reclaimed;
static uint32_t allocation_reclaimed_bytes;
static unsigned reason,allocations;
static void *pool=(void *)1;
static size_t heap_caps_get_free_size(int caps){(void)caps;return available;}
static size_t xip_pool_reclaim(void *p,size_t bytes){assert(p==pool);size_t n=bytes<cached?bytes:cached;available+=n;cached-=n;reclaimed+=n;return n;}
static void module_allocation_failed(struct instance *in,size_t size,unsigned why){(void)in;(void)size;reason=why;}
static void *xip_mem_alloc(struct memory *mem,size_t size){if(size>mem->quota-mem->used)return NULL;allocations++;available-=size+31;mem->used+=size;return malloc(size);}
static void xip_mem_free(struct memory *mem,void *p){free(p);(void)mem;}
#include "../../components/xip_runtime/src/module_alloc.inc"
int main(void)
{
    available=32768;cached=1024;reclaimed=0;
    void *file=memory_internal_alloc(100);assert(file && reclaimed==131 && available>=32768);free(file);
    available=32768;cached=0;assert(memory_internal_alloc(1)==NULL);

    struct instance in={.mem={0,10000}};
    assert(XIP_PACKET_HEAP_RESERVE==32768);
    available=21056;cached=20000;reclaimed=0;
    void *p=h_mem_alloc(&in,80);
    assert(p && !reason && allocations==1 && reclaimed==11823);
    assert(available==32768 && allocation_reclaimed_bytes==11823);
    free(p);
    available=21056;cached=4000;reason=0;
    assert(!h_mem_alloc(&in,80) && reason==1 && allocations==1);
    assert(available==25056 && allocation_reclaimed_bytes==4000);
    available=30000;cached=10000;reason=0;
    assert(!h_mem_alloc(&in,SIZE_MAX) && reason==1 && allocations==1);
    assert(!allocation_reclaimed_bytes && available==30000 && cached==10000);
    puts("module reserve: reclaim cache before admission, preserve floor, overflow rejection ok");
}
