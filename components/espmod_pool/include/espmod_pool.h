/* SPDX-License-Identifier: Apache-2.0 */
/* The packet pool of the C runtime, and the table of packets a module holds. No ESP-IDF in here: it builds on a PC for the host
 * tests. What every runtime must agree on is the contract in abi/API.md and docs/packets.md, not this code.
 *
 * Slots with reference counts, the way QUIC-lite's heap pool does it: a table of at most `slots` entries, each slot's storage
 * allocated when it is first needed and given back when more than `keep` idle slots hold storage, so a device whose modules are
 * quiet holds almost nothing. A host that already has such a pool (DMesh: the QUIC pool) offers it through espmod_pool_ops and the
 * same calls go to it. */
#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct espmod_pool espmod_pool;

/* A pool owned by someone else. A slot is an index below slots(); acquire gives it one reference; the last release frees it.
 * `reserve` slots stay free for the application's own traffic (the caller that may use everything passes 0). All calls may come
 * from any task. */
typedef struct {
    void *ctx;
    int (*acquire)(void *ctx, uint16_t reserve);        /* a slot with one reference, or -1 */
    void (*retain)(void *ctx, int slot);
    void (*release)(void *ctx, int slot);
    uint32_t (*refs)(void *ctx, int slot);
    uint8_t *(*data)(void *ctx, int slot);              /* the slot's first byte; NULL if it has no storage */
    uint16_t (*slot_size)(void *ctx);                   /* bytes; at least ESPMOD_PKT_SLOT_MIN */
    uint16_t (*slots)(void *ctx);                       /* the table size */
    uint16_t (*available)(void *ctx);
} espmod_pool_ops;

/* A pool of up to `slots` slots of `slot_size` bytes (at least ESPMOD_PKT_SLOT_MIN for the packets of the ABI); no storage yet.
 * NULL when out of memory. */
espmod_pool *espmod_pool_create(uint16_t slots, uint16_t slot_size);
/* The pool behind `ops` (which must outlive it). */
espmod_pool *espmod_pool_external(const espmod_pool_ops *ops);
void espmod_pool_destroy(espmod_pool *pool);
/* Storage policy of a created pool: keep `keep` idle slots with storage (default 2), and do not allocate storage when
 * free_heap() (if given) reports less than slot_size + reserve_bytes. */
void espmod_pool_set_policy(espmod_pool *pool, uint16_t keep, size_t (*free_heap)(void), size_t reserve_bytes);
/* Give back the storage of idle slots beyond `keep` (after the policy was lowered). */
void espmod_pool_trim(espmod_pool *pool);
/* Slots that hold storage now. */
uint16_t espmod_pool_allocated(const espmod_pool *pool);

/* A free slot with one reference, or -1 (none free beyond `reserve`, or no memory for its storage). */
int espmod_pool_acquire(espmod_pool *pool, uint16_t reserve);
void espmod_pool_retain(espmod_pool *pool, int slot);
/* The last release frees the slot (and its storage, if more than `keep` idle slots have some). */
void espmod_pool_release(espmod_pool *pool, int slot);
uint32_t espmod_pool_refs(const espmod_pool *pool, int slot);
uint8_t *espmod_pool_slot(espmod_pool *pool, int slot);
uint16_t espmod_pool_slot_size(const espmod_pool *pool);
uint16_t espmod_pool_available(const espmod_pool *pool);
uint16_t espmod_pool_slots(const espmod_pool *pool);

/* ---- what one module holds ---- */
#define ESPMOD_HELD_MAX 8

typedef enum { ESPMOD_HELD_FREE = 0, ESPMOD_HELD_OWNED, ESPMOD_HELD_BORROWED } espmod_held_state;

typedef struct {
    int16_t slot;
    uint16_t start, len;          /* the payload inside the slot: the range belongs to the holder */
    uint8_t gen, state;           /* the handle carries gen: a stale one does not match */
} espmod_held_entry;

typedef struct {
    espmod_held_entry e[ESPMOD_HELD_MAX];
    uint8_t quota;                /* owned packets at most (<= ESPMOD_HELD_MAX); borrowed ones are the call's */
} espmod_held;

void espmod_held_init(espmod_held *h, uint8_t quota);
/* A handle (never 0) for a slot the table now owns a reference of; 0 when the table is full or, for an owned packet, at
 * its quota. */
uint32_t espmod_held_add(espmod_held *h, int slot, uint16_t start, uint16_t len, espmod_held_state state);
espmod_held_entry *espmod_held_get(espmod_held *h, uint32_t handle);          /* NULL: not valid */
/* Borrowed -> owned. 0, or -1 for a bad handle, -2 at the quota. */
int espmod_held_keep(espmod_held *h, uint32_t handle);
/* The entry leaves the table; the caller releases or hands on the slot reference. Returns a copy, slot -1 if not valid. */
espmod_held_entry espmod_held_take(espmod_held *h, uint32_t handle);
unsigned espmod_held_count(const espmod_held *h, espmod_held_state state);

/* ---- the managed heap of one module: the bytes it has taken and not given back ----
 * Only the balance is tracked, not the blocks: a module that stops with a balance above zero cannot be replaced live (the
 * host cannot know which memory is left), the device has to restart. */
typedef struct {
    uint32_t used, quota;         /* bytes */
} espmod_mem;

void espmod_mem_init(espmod_mem *m, uint32_t quota);
void *espmod_mem_alloc(espmod_mem *m, size_t size);          /* NULL: over the quota or no heap; aligned for any type */
int espmod_mem_free(espmod_mem *m, void *ptr);                /* -1 for a pointer that is not from espmod_mem_alloc (best effort) */
uint32_t espmod_mem_used(const espmod_mem *m);

#ifdef __cplusplus
}
#endif
