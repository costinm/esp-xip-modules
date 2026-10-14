/* SPDX-License-Identifier: Apache-2.0 */
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include "espmod_pool.h"

struct espmod_pool {
    const espmod_pool_ops *ext;
    uint16_t slots, slot_size, keep;
    atomic_uint free_count;        /* slots not in use */
    atomic_uint buffers;           /* slots that hold storage (in use or idle) */
    atomic_uint *refs;
    uint8_t **mem;
    size_t (*free_heap)(void);
    size_t reserve_bytes;
};

espmod_pool *espmod_pool_create(uint16_t slots, uint16_t slot_size)
{
    if (!slots || !slot_size) return NULL;
    espmod_pool *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->slots = slots;
    p->slot_size = slot_size;
    p->keep = 2;
    p->refs = calloc(slots, sizeof(*p->refs));
    p->mem = calloc(slots, sizeof(*p->mem));
    if (!p->refs || !p->mem) { espmod_pool_destroy(p); return NULL; }
    atomic_init(&p->free_count, slots);
    atomic_init(&p->buffers, 0);
    for (uint16_t i = 0; i < slots; i++) atomic_init(&p->refs[i], 0);
    return p;
}

espmod_pool *espmod_pool_external(const espmod_pool_ops *ops)
{
    if (!ops || !ops->acquire || !ops->retain || !ops->release || !ops->refs || !ops->data || !ops->slot_size || !ops->slots || !ops->available) return NULL;
    espmod_pool *p = calloc(1, sizeof(*p));
    if (p) p->ext = ops;
    return p;
}

void espmod_pool_destroy(espmod_pool *p)
{
    if (!p) return;
    if (!p->ext && p->mem) for (uint16_t i = 0; i < p->slots; i++) free(p->mem[i]);
    free(p->refs);
    free(p->mem);
    free(p);
}

void espmod_pool_set_policy(espmod_pool *p, uint16_t keep, size_t (*free_heap)(void), size_t reserve_bytes)
{
    if (!p || p->ext) return;
    p->keep = keep;
    p->free_heap = free_heap;
    p->reserve_bytes = reserve_bytes;
}

void espmod_pool_trim(espmod_pool *p)
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

uint16_t espmod_pool_allocated(const espmod_pool *p) { return p->ext ? p->ext->slots(p->ext->ctx) - p->ext->available(p->ext->ctx) : (uint16_t)atomic_load(&((espmod_pool *)p)->buffers); }

/* Claim slot i (its count 0 -> 1) and make sure it has storage; undo and fail when it cannot. */
static int claim(espmod_pool *p, uint16_t i)
{
    unsigned zero = 0;
    if (!atomic_compare_exchange_strong(&p->refs[i], &zero, 1)) return 0;
    if (!p->mem[i]) {
        uint8_t *m = NULL;
        if (!p->free_heap || p->free_heap() >= (size_t)p->slot_size + p->reserve_bytes) m = malloc(p->slot_size);
        if (!m) { atomic_store(&p->refs[i], 0); return -1; }
        p->mem[i] = m;
        atomic_fetch_add(&p->buffers, 1);
    }
    return 1;
}

int espmod_pool_acquire(espmod_pool *p, uint16_t reserve)
{
    if (p->ext) return p->ext->acquire(p->ext->ctx, reserve);
    /* take one from the count only if more than `reserve` are free, in one step: two takers cannot both pass */
    unsigned n = atomic_load(&p->free_count);
    do {
        if (n <= reserve) return -1;
    } while (!atomic_compare_exchange_weak(&p->free_count, &n, n - 1));
    for (int pass = 0; pass < 2; pass++) {              /* slots that already have storage first, then any */
        for (uint16_t i = 0; i < p->slots; i++) {
            if (pass == 0 && !p->mem[i]) continue;
            int c = claim(p, i);
            if (c > 0) return i;
            if (c < 0) { atomic_fetch_add(&p->free_count, 1); return -1; }      /* no memory for the storage */
        }
    }
    atomic_fetch_add(&p->free_count, 1);     /* cannot happen: the count said a slot was free */
    return -1;
}

void espmod_pool_retain(espmod_pool *p, int slot)
{
    if (p->ext) { p->ext->retain(p->ext->ctx, slot); return; }
    if (slot >= 0 && slot < p->slots) atomic_fetch_add(&p->refs[slot], 1);
}

void espmod_pool_release(espmod_pool *p, int slot)
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

uint32_t espmod_pool_refs(const espmod_pool *p, int slot)
{
    if (p->ext) return p->ext->refs(p->ext->ctx, slot);
    return slot >= 0 && slot < p->slots ? atomic_load(&((espmod_pool *)p)->refs[slot]) : 0;
}

uint8_t *espmod_pool_slot(espmod_pool *p, int slot)
{
    if (p->ext) return p->ext->data(p->ext->ctx, slot);
    return slot >= 0 && slot < p->slots ? p->mem[slot] : NULL;
}

uint16_t espmod_pool_slot_size(const espmod_pool *p) { return p->ext ? p->ext->slot_size(p->ext->ctx) : p->slot_size; }
uint16_t espmod_pool_slots(const espmod_pool *p) { return p->ext ? p->ext->slots(p->ext->ctx) : p->slots; }
uint16_t espmod_pool_available(const espmod_pool *p) { return p->ext ? p->ext->available(p->ext->ctx) : (uint16_t)atomic_load(&((espmod_pool *)p)->free_count); }

/* ---- held ---- */
void espmod_held_init(espmod_held *h, uint8_t quota)
{
    memset(h, 0, sizeof(*h));
    h->quota = quota > ESPMOD_HELD_MAX ? ESPMOD_HELD_MAX : quota;
}

static uint32_t handle_of(const espmod_held *h, unsigned i) { return (uint32_t)h->e[i].gen << 8 | (i + 1); }

unsigned espmod_held_count(const espmod_held *h, espmod_held_state state)
{
    unsigned n = 0;
    for (unsigned i = 0; i < ESPMOD_HELD_MAX; i++) n += h->e[i].state == state;
    return n;
}

uint32_t espmod_held_add(espmod_held *h, int slot, uint16_t start, uint16_t len, espmod_held_state state)
{
    if (state == ESPMOD_HELD_OWNED && espmod_held_count(h, ESPMOD_HELD_OWNED) >= h->quota) return 0;
    for (unsigned i = 0; i < ESPMOD_HELD_MAX; i++) {
        if (h->e[i].state != ESPMOD_HELD_FREE) continue;
        h->e[i].slot = (int16_t)slot;
        h->e[i].start = start;
        h->e[i].len = len;
        h->e[i].state = (uint8_t)state;
        if (++h->e[i].gen == 0) h->e[i].gen = 1;     /* 0 would make handle 0x00NN: keep generation >= 1 */
        return handle_of(h, i);
    }
    return 0;
}

espmod_held_entry *espmod_held_get(espmod_held *h, uint32_t handle)
{
    unsigned i = (handle & 0xff);
    if (i == 0 || i > ESPMOD_HELD_MAX) return NULL;
    espmod_held_entry *e = &h->e[i - 1];
    return e->state != ESPMOD_HELD_FREE && handle == handle_of(h, i - 1) ? e : NULL;
}

int espmod_held_keep(espmod_held *h, uint32_t handle)
{
    espmod_held_entry *e = espmod_held_get(h, handle);
    if (!e) return -1;
    if (e->state == ESPMOD_HELD_OWNED) return 0;
    if (espmod_held_count(h, ESPMOD_HELD_OWNED) >= h->quota) return -2;
    e->state = ESPMOD_HELD_OWNED;
    return 0;
}

espmod_held_entry espmod_held_take(espmod_held *h, uint32_t handle)
{
    espmod_held_entry none = {.slot = -1};
    espmod_held_entry *e = espmod_held_get(h, handle);
    if (!e) return none;
    espmod_held_entry copy = *e;
    e->state = ESPMOD_HELD_FREE;                       /* gen stays: the old handle no longer matches the next use */
    return copy;
}

/* ---- managed heap ---- */
#define MEM_HEADER 16u                      /* keeps the block aligned for any type */
#define MEM_MAGIC 0x6d656d31u

void espmod_mem_init(espmod_mem *m, uint32_t quota)
{
    m->used = 0;
    m->quota = quota;
}

/* Before every block: its size, a magic and where malloc's block starts. The block is aligned to 16: malloc itself is only
 * 4-aligned on the Xtensa chips, and a module (Rust: u64, f64; C: long long) may be given any type. */
typedef struct { uint32_t size, magic; uintptr_t raw; } mem_head;

void *espmod_mem_alloc(espmod_mem *m, size_t size)
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

int espmod_mem_free(espmod_mem *m, void *ptr)
{
    if (!ptr) return -1;
    mem_head *h = (mem_head *)((uint8_t *)ptr - MEM_HEADER);
    if (h->magic != MEM_MAGIC || h->size > m->used) return -1;
    m->used -= h->size;
    h->magic = 0;
    free((void *)h->raw);
    return 0;
}

uint32_t espmod_mem_used(const espmod_mem *m) { return m->used; }
