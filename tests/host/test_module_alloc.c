/* Production managed-allocation admission and rollback, with controlled SDK heap readings. */
#include <assert.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include "xip_pool.h"
#include "../../components/xip_runtime/src/xip_state.c"
#define MALLOC_CAP_DEFAULT 0
struct xip_instance { xip_mem mem; };
static struct xip_instance *I(void *user){return user;}
static size_t available[2];
static unsigned reads,failures,last_reason;
static size_t heap_caps_get_free_size(int caps){(void)caps;return available[reads++?1:0];}
static void module_allocation_failed(struct xip_instance *in,size_t size,unsigned reason){(void)in;assert(size==100);failures++;last_reason=reason;}
#include "../../components/xip_runtime/src/module_alloc.inc"
static void heap(size_t before,size_t after){available[0]=before;available[1]=after;reads=failures=last_reason=0;}
/* State objects (xip_state.c): instance objects in the quota, the root, retained objects per name and version, the caps. */
static void state_objects(void)
{
    xip_mem mem; xip_mem_init(&mem, 1024);
    xip_state_set set = {0};
    xip_retained_store ret = {0}; ret.cap = 256;
    uint8_t *p; size_t n;
    void *q;
    /* create (zeroed), grow keeps the contents, shrink keeps the head, same size keeps the pointer */
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 1, XIP_STATE_INSTANCE, 100, (uint8_t **)&p) == XIP_OK && p);
    for (size_t i = 0; i < 100; i++) assert(p[i] == 0);
    memset(p, 0x5a, 100); assert(mem.used == 100);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 1, XIP_STATE_INSTANCE, 300, (uint8_t **)&q) == XIP_OK);
    p = q; assert(mem.used == 300 && p[0] == 0x5a && p[99] == 0x5a && p[100] == 0 && p[299] == 0);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 1, XIP_STATE_INSTANCE, 300, (uint8_t **)&q) == XIP_OK && q == p);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 1, XIP_STATE_INSTANCE, 50, (uint8_t **)&q) == XIP_OK);
    p = q; assert(mem.used == 50 && p[0] == 0x5a && p[49] == 0x5a);
    assert(xip_state_get(&set, &ret, "m", 1, 1, (uint8_t **)&q, &n) == XIP_OK && q == p && n == 50);
    /* free: the heap balance goes back, the key is gone */
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 1, XIP_STATE_INSTANCE, 0, (uint8_t **)&q) == XIP_OK && !q);
    assert(mem.used == 0 && xip_state_get(&set, &ret, "m", 1, 1, NULL, NULL) == XIP_ERR_NOTFOUND);
    /* quota: the new size counts, the old one does not (a grow of 1024 bytes over 512 fails and keeps the old) */
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 2, XIP_STATE_INSTANCE, 512, (uint8_t **)&p) == XIP_OK);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 3, XIP_STATE_INSTANCE, 2048, (uint8_t **)&q) == XIP_ERR_SIZE && !q);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 3, XIP_STATE_INSTANCE, 500, (uint8_t **)&q) == XIP_OK && mem.used == 1012);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 3, XIP_STATE_INSTANCE, 600, (uint8_t **)&q) == XIP_ERR_SIZE && mem.used == 1012);
    assert(xip_state_get(&set, &ret, "m", 1, 3, (uint8_t **)&q, &n) == XIP_OK && n == 500);
    /* class mismatch and reserved classes */
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 2, XIP_STATE_RETAINED, 8, (uint8_t **)&q) == XIP_ERR_ARG);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 9, XIP_STATE_PSRAM, 8, (uint8_t **)&q) == XIP_ERR_UNSUPPORTED);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 9, XIP_STATE_RTC, 8, (uint8_t **)&q) == XIP_ERR_UNSUPPORTED);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 9, 7, 8, (uint8_t **)&q) == XIP_ERR_ARG);
    /* the root: the image's size, not counted, a resize to another size is unsupported, a retained root is an error */
    uint8_t *root = xip_state_set_root(&set, 32);
    assert(root && xip_state_get(&set, &ret, "m", 1, 0, (uint8_t **)&q, &n) == XIP_OK && q == root && n == 32);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 0, XIP_STATE_INSTANCE, 32, (uint8_t **)&q) == XIP_OK && q == root);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 0, XIP_STATE_INSTANCE, 64, (uint8_t **)&q) == XIP_ERR_UNSUPPORTED);
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 0, XIP_STATE_RETAINED, 8, (uint8_t **)&q) == XIP_ERR_ARG);
    assert(mem.used == 1012);
    /* retained objects: kept after the instance is released, for the same name and version only */
    assert(xip_state_resize(&set, &mem, &ret, "m", 1, 7, XIP_STATE_RETAINED, 100, (uint8_t **)&p) == XIP_OK);
    memset(p, 0x33, 100);
    assert(ret.bytes == 100 && mem.used == 1012);
    xip_state_release_instance(&set, &mem);
    assert(mem.used == 0 && set.objs == NULL);
    xip_state_set next = {0};
    assert(xip_state_get(&next, &ret, "m", 1, 7, (uint8_t **)&q, &n) == XIP_OK && n == 100 && ((uint8_t *)q)[99] == 0x33);
    assert(xip_state_resize(&next, &mem, &ret, "m", 1, 7, XIP_STATE_RETAINED, 40, (uint8_t **)&q) == XIP_OK && ret.bytes == 40);
    assert(xip_state_get(&next, &ret, "other", 1, 7, NULL, NULL) == XIP_ERR_NOTFOUND);
    /* a start with another version drops the retained objects of the name */
    xip_retained_forget_other_versions(&ret, "m", 2);
    assert(xip_state_get(&next, &ret, "m", 1, 7, NULL, NULL) == XIP_ERR_NOTFOUND && ret.bytes == 0 && ret.mods == NULL);
    /* the caps: 256 bytes of retained objects in all, 8 objects per name */
    assert(xip_state_resize(&next, &mem, &ret, "m", 2, 7, XIP_STATE_RETAINED, 257, (uint8_t **)&q) == XIP_ERR_SIZE);
    for (uint32_t k = 1; k <= XIP_STATE_RETAINED_OBJECTS; k++)
        assert(xip_state_resize(&next, &mem, &ret, "m", 2, k, XIP_STATE_RETAINED, 8, (uint8_t **)&q) == XIP_OK);
    assert(ret.bytes == 64);
    assert(xip_state_resize(&next, &mem, &ret, "m", 2, 99, XIP_STATE_RETAINED, 8, (uint8_t **)&q) == XIP_ERR_SIZE);
    assert(xip_state_resize(&next, &mem, &ret, "n", 2, 1, XIP_STATE_RETAINED, 8, (uint8_t **)&q) == XIP_OK);   /* another name has its own */
    xip_retained_forget_other_versions(&ret, "m", 3);     /* the version 2 objects of "m" go; "n" stays */
    assert(ret.bytes == 8 && xip_state_get(&next, &ret, "n", 2, 1, NULL, NULL) == XIP_OK);
    /* the cleanup of a store and a set leaves nothing behind (ASan reports leaks at exit) */
    assert(xip_state_resize(&next, &mem, &ret, "n", 2, 1, XIP_STATE_RETAINED, 0, (uint8_t **)&q) == XIP_OK && ret.bytes == 0 && ret.mods == NULL);
    /* the dynamic root (key 0 without a header block) is resizable and not charged to the quota */
    xip_state_set dyn = {0};
    assert(xip_state_resize(&dyn, &mem, &ret, "m", 1, 0, XIP_STATE_INSTANCE, 40, (uint8_t **)&q) == XIP_OK && q && mem.used == 0);
    assert(xip_state_resize(&dyn, &mem, &ret, "m", 1, 0, XIP_STATE_INSTANCE, 4000, (uint8_t **)&q) == XIP_OK && mem.used == 0);
    assert(xip_state_get(&dyn, &ret, "m", 1, 0, NULL, &n) == XIP_OK && n == 4000);
    xip_state_release_instance(&dyn, &mem);
    xip_state_release_instance(&next, &mem);
    puts("state objects: classes, grow/shrink/free, quota and root, retained per name and version, caps and reserved classes ok");
}

int main(void){
    struct xip_instance in;xip_mem_init(&in.mem,1024);
    heap(32768,32768);assert(!h_mem_alloc(&in,100));assert(in.mem.used==0 && failures==1 && last_reason==1);
    heap(32800,32800);assert(!h_mem_alloc(&in,100));assert(in.mem.used==0 && failures==1 && last_reason==1);
    heap(40000,32000);assert(!h_mem_alloc(&in,100));assert(in.mem.used==0 && failures==1 && last_reason==3);
    xip_mem_init(&in.mem,64);heap(40000,40000);assert(!h_mem_alloc(&in,100));assert(in.mem.used==0 && failures==1 && last_reason==2);
    xip_mem_init(&in.mem,1024);heap(40000,39800);void *p=h_mem_alloc(&in,100);assert(p && in.mem.used==100 && !failures);
    assert(!xip_mem_free(&in.mem,p) && in.mem.used==0);
    puts("managed module allocation: reserve, quota, post-allocation rollback diagnostics and success ownership ok");
    state_objects();
}
