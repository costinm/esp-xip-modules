/* SPDX-License-Identifier: Apache-2.0 */
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include "xip_pool.h"

struct xip_pool {
    const xip_pool_ops *ext;
    uint16_t slots, slot_size;
    atomic_uint keep;
    atomic_uint free_count;        /* slots not in use */
    atomic_uint buffers;           /* slots that hold storage (in use or idle) */
    atomic_uint *refs;
    _Atomic(uint8_t *) *mem;
    atomic_uint *generations;
    xip_packet_ref *packets;
    size_t (*free_heap)(void);
    size_t reserve_bytes;
};

xip_pool *xip_pool_create(uint16_t slots, uint16_t slot_size)
{
    if (!slots || !slot_size) return NULL;
    xip_pool *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->slots = slots;
    p->slot_size = slot_size;
    atomic_init(&p->keep, 2);
    p->generations = calloc(slots, sizeof(*p->generations));
    p->packets = calloc(slots, sizeof(*p->packets));
    p->refs = calloc(slots, sizeof(*p->refs));
    p->mem = calloc(slots, sizeof(*p->mem));
    if (!p->refs || !p->mem || !p->generations || !p->packets) { xip_pool_destroy(p); return NULL; }
    atomic_init(&p->free_count, slots);
    atomic_init(&p->buffers, 0);
    for (uint16_t i = 0; i < slots; i++) {
        atomic_init(&p->refs[i], 0);
        atomic_init(&p->mem[i], NULL);
    }
    return p;
}

xip_pool *xip_pool_external(const xip_pool_ops *ops)
{
    if (!ops || !ops->acquire || !ops->retain || !ops->release || !ops->refs || !ops->data || !ops->slot_size || !ops->slots || !ops->available) return NULL;
    xip_pool *p = calloc(1, sizeof(*p));
    if (p) {
        p->ext = ops;p->slots = ops->slots(ops->ctx);
        p->generations=calloc(p->slots,sizeof(*p->generations));
        p->packets=calloc(p->slots,sizeof(*p->packets));
        if(!p->generations || !p->packets){xip_pool_destroy(p);return NULL;}
    }
    return p;
}

void xip_pool_destroy(xip_pool *p)
{
    if (!p) return;
    if (!p->ext && p->mem) for (uint16_t i = 0; i < p->slots; i++) free(p->mem[i]);
    free(p->generations);free(p->packets);
    free(p->refs);
    free(p->mem);
    free(p);
}

void xip_pool_set_policy(xip_pool *p, uint16_t keep, size_t (*free_heap)(void), size_t reserve_bytes)
{
    if (!p || p->ext) return;
    p->keep = keep;
    p->free_heap = free_heap;
    p->reserve_bytes = reserve_bytes;
}

void xip_pool_set_keep(xip_pool *p,uint16_t keep)
{
    if(p && !p->ext)atomic_store(&p->keep,keep);
}

void xip_pool_trim(xip_pool *p)
{
    if (!p || p->ext) return;
    for (uint16_t i = 0; i < p->slots; i++) {
        if (atomic_load(&p->buffers) <= p->keep + (p->slots - atomic_load(&p->free_count))) return;
        unsigned zero = 0;
        if (!p->mem[i] || !atomic_compare_exchange_strong(&p->refs[i], &zero, 1)) continue;       /* idle: take it to free its storage */
        free(p->mem[i]);
        p->mem[i] = NULL;
        atomic_fetch_sub(&p->buffers, 1);
        atomic_store(&p->refs[i], 0);
    }
}

uint16_t xip_pool_allocated(const xip_pool *p) { return p->ext ? p->ext->slots(p->ext->ctx) - p->ext->available(p->ext->ctx) : (uint16_t)atomic_load(&((xip_pool *)p)->buffers); }

/* Claim slot i (its count 0 -> 1) and make sure it has storage; undo and fail when it cannot. */
static int claim(xip_pool *p, uint16_t i, int allocate)
{
    unsigned zero = 0;
    if (!atomic_compare_exchange_strong(&p->refs[i], &zero, 1)) return 0;
    if (allocate==2 && p->mem[i]) { atomic_store(&p->refs[i],0); return 0; }
    if (!p->mem[i]) {
        if (!allocate) { atomic_store(&p->refs[i], 0); return 0; }
        uint8_t *m = NULL;
        if (!p->free_heap || p->free_heap() >= (size_t)p->slot_size + p->reserve_bytes) m = malloc(p->slot_size);
        if (!m) { atomic_store(&p->refs[i], 0); return -1; }
        p->mem[i] = m;
        atomic_fetch_add(&p->buffers, 1);
    }
    return 1;
}

static int acquire_slot(xip_pool *p, uint16_t reserve, int allocate)
{
    if (p->ext) {
        int slot=allocate ? p->ext->acquire(p->ext->ctx,reserve) :
            (p->ext->acquire_cached ? p->ext->acquire_cached(p->ext->ctx,reserve) : -1);
        if(slot>=0 && slot<p->slots) atomic_fetch_add(&p->generations[slot],1);
        return slot;
    }
    /* take one from the count only if more than `reserve` are free, in one step: two takers cannot both pass */
    unsigned n = atomic_load(&p->free_count);
    do {
        if (n <= reserve) return -1;
    } while (!atomic_compare_exchange_weak(&p->free_count, &n, n - 1));
    for (int pass = allocate==2 ? 1 : 0; pass < 2; pass++) { /* cached first, or unbacked only for owner preparation */
        for (uint16_t i = 0; i < p->slots; i++) {
            if (pass == 0 && !p->mem[i]) continue;
            if (allocate==2 && p->mem[i]) continue;
            int c = claim(p, i, allocate);
            if (c > 0) {atomic_fetch_add(&p->generations[i],1);return i;}
            if (c < 0) { atomic_fetch_add(&p->free_count, 1); return -1; }      /* no memory for the storage */
        }
    }
    atomic_fetch_add(&p->free_count, 1);     /* cannot happen: the count said a slot was free */
    return -1;
}

int xip_pool_acquire(xip_pool *p, uint16_t reserve) { return acquire_slot(p, reserve, 1); }
int xip_pool_acquire_cached(xip_pool *p, uint16_t reserve) { return acquire_slot(p, reserve, 0); }
uint16_t xip_pool_cached_available(const xip_pool *p)
{
    if(!p)return 0;
    if(p->ext)return 0; /* External providers do not expose cached-storage counts. */
    unsigned n=0;
    for(uint16_t i=0;i<p->slots;i++)if(p->mem[i] && !atomic_load(&p->refs[i]))n++;
    return (uint16_t)n;
}
void xip_pool_prepare_rx(xip_pool *p)
{
    if (p->ext) return; /* The application's owner prepares its pool. */
    unsigned target=atomic_load(&p->keep);
    if(target>32)target=32;
    /* Never sequester the ready cache while allocating. Publish each new idle
     * buffer immediately, so callback RX can continue during owner preparation. */
    for(unsigned i=0;i<p->slots && xip_pool_cached_available(p)<target;i++){
        int slot=acquire_slot(p,0,2); /* Only slots without storage. */
        if(slot<0)break;
        xip_pool_release(p,slot);
    }
}

void xip_pool_retain(xip_pool *p, int slot)
{
    if (p->ext) { p->ext->retain(p->ext->ctx, slot); return; }
    if (slot >= 0 && slot < p->slots) atomic_fetch_add(&p->refs[slot], 1);
}

void xip_pool_release(xip_pool *p, int slot)
{
    if (p->ext) { p->ext->release(p->ext->ctx, slot); return; }
    if (slot < 0 || slot >= p->slots) return;
    unsigned r = atomic_load(&p->refs[slot]);
    for (;;) {
        if (r == 0) return;                  /* not in use: a double release is ignored, not counted */
        if (r > 1) {
            if (atomic_compare_exchange_weak(&p->refs[slot], &r, r - 1)) return;
            continue;
        }
        /* the last reference: give the storage back first if this would leave more than `keep` idle slots with some, then free the
         * slot (nobody can claim it before its count is 0, so nobody uses the storage meanwhile) */
        unsigned in_use = p->slots - atomic_load(&p->free_count), buffers = atomic_load(&p->buffers);
        if (buffers + 1 > in_use + p->keep && p->mem[slot]) {
            free(p->mem[slot]);
            p->mem[slot] = NULL;
            atomic_fetch_sub(&p->buffers, 1);
        }
        atomic_store(&p->refs[slot], 0);
        atomic_fetch_add(&p->free_count, 1);
        return;
    }
}

uint32_t xip_pool_refs(const xip_pool *p, int slot)
{
    if (p->ext) return p->ext->refs(p->ext->ctx, slot);
    return slot >= 0 && slot < p->slots ? atomic_load(&((xip_pool *)p)->refs[slot]) : 0;
}

uint8_t *xip_pool_slot(xip_pool *p, int slot)
{
    if (p->ext) return p->ext->data(p->ext->ctx, slot);
    return slot >= 0 && slot < p->slots ? p->mem[slot] : NULL;
}

uint16_t xip_pool_slot_size(const xip_pool *p) { return p->ext ? p->ext->slot_size(p->ext->ctx) : p->slot_size; }
uint16_t xip_pool_slots(const xip_pool *p) { return p->ext ? p->ext->slots(p->ext->ctx) : p->slots; }
uint16_t xip_pool_available(const xip_pool *p) { return p->ext ? p->ext->available(p->ext->ctx) : (uint16_t)atomic_load(&((xip_pool *)p)->free_count); }

/* Pool handles carry allocation generation, not an owner/module identity. */
uint32_t xip_packet_handle(xip_pool *p,int slot,uint16_t start,uint16_t len)
{
    if(!p || slot<0 || slot>=p->slots || !xip_pool_refs(p,slot))return 0;
    p->packets[slot]=(xip_packet_ref){.slot=slot,.start=start,.len=len};
    unsigned g=atomic_load(&p->generations[slot]);
    return ((g & 0xffffu)<<16) | (unsigned)(slot+1);
}
xip_packet_ref *xip_packet_get(xip_pool *p,uint32_t handle)
{
    unsigned index=(handle & 0xffffu);
    if(!p || !index || index>p->slots)return NULL;
    unsigned slot=index-1;
    if(!xip_pool_refs(p,slot) || (atomic_load(&p->generations[slot]) & 0xffffu)!=(handle>>16))return NULL;
    return &p->packets[slot];
}
int xip_packet_retain(xip_pool *p,uint32_t handle)
{
    xip_packet_ref *packet=xip_packet_get(p,handle);
    if(!packet)return -1;
    xip_pool_retain(p,packet->slot);return 0;
}

/* ---- managed heap ---- */
#define MEM_HEADER 16u                      /* keeps the block aligned for any type */
#define MEM_MAGIC 0x6d656d31u

void xip_mem_init(xip_mem *m, uint32_t quota)
{
    m->used = 0;
    m->quota = quota;
}

/* Before every block: its size, a magic and where malloc's block starts. The block is aligned to 16: malloc itself is only
 * 4-aligned on the Xtensa chips, and a module (Rust: u64, f64; C: long long) may be given any type. */
typedef struct { uint32_t size, magic; uintptr_t raw; } mem_head;

void *xip_mem_alloc(xip_mem *m, size_t size)
{
    if (!size || size > SIZE_MAX - MEM_HEADER - 15 || size > m->quota || m->used > m->quota - size) return NULL;
    uint8_t *raw = malloc(size + MEM_HEADER + 15);
    if (!raw) return NULL;
    uint8_t *user = (uint8_t *)(((uintptr_t)raw + MEM_HEADER + 15) & ~(uintptr_t)15);
    mem_head *h = (mem_head *)(user - MEM_HEADER);
    h->size = (uint32_t)size;
    h->magic = MEM_MAGIC;
    h->raw = (uintptr_t)raw;
    m->used += (uint32_t)size;
    return user;
}

int xip_mem_free(xip_mem *m, void *ptr)
{
    if (!ptr) return -1;
    mem_head *h = (mem_head *)((uint8_t *)ptr - MEM_HEADER);
    if (h->magic != MEM_MAGIC || h->size > m->used) return -1;
    m->used -= h->size;
    h->magic = 0;
    free((void *)h->raw);
    return 0;
}

uint32_t xip_mem_used(const xip_mem *m) { return m->used; }
