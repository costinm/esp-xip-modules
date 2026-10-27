/* SPDX-License-Identifier: Apache-2.0 */
/* The packet pool of the C runtime, and the table of packets a module holds. No ESP-IDF in here: it builds on a PC for the host
 * tests. What every runtime must agree on is the contract in include/xip.h (PKT) and docs/packets.md, not this code.
 *
 * Slots with reference counts, the way QUIC-lite's heap pool does it: a table of at most `slots` entries, each slot's storage
 * allocated when it is first needed and given back when more than `keep` idle slots hold storage, so a device whose modules are
 * quiet holds almost nothing. A host that already has such a pool (DMesh: the QUIC pool) offers it through xip_pool_ops and the
 * same calls go to it. */
#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct xip_pool xip_pool;

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
    uint16_t (*slot_size)(void *ctx);                   /* bytes; at least XIP_PKT_SLOT_MIN */
    uint16_t (*slots)(void *ctx);                       /* the table size */
    uint16_t (*available)(void *ctx);
    /* Optional: must never allocate storage; -1 means drop when none is ready. */
    int (*acquire_cached)(void *ctx, uint16_t reserve);
} xip_pool_ops;

/* A pool of up to `slots` slots of `slot_size` bytes (at least XIP_PKT_SLOT_MIN for the packets of the ABI); no storage yet.
 * NULL when out of memory. */
xip_pool *xip_pool_create(uint16_t slots, uint16_t slot_size);
/* The pool behind `ops` (which must outlive it). */
xip_pool *xip_pool_external(const xip_pool_ops *ops);
void xip_pool_destroy(xip_pool *pool);
/* Storage policy of a created pool: keep `keep` idle slots with storage (default 2), and do not allocate storage when
 * free_heap() (if given) reports less than slot_size + reserve_bytes. */
void xip_pool_set_policy(xip_pool *pool, uint16_t keep, size_t (*free_heap)(void), size_t reserve_bytes);
/* Change only the idle cache target while callbacks are using the pool. */
void xip_pool_set_keep(xip_pool *pool, uint16_t keep);
/* Give back the storage of idle slots beyond `keep` (after the policy was lowered). */
void xip_pool_trim(xip_pool *pool);
/* Slots that hold storage now. */
uint16_t xip_pool_allocated(const xip_pool *pool);

/* A free slot with one reference, or -1 (none free beyond `reserve`, or no memory for its storage). */
int xip_pool_acquire(xip_pool *pool, uint16_t reserve);
/* Callback-safe admission: never malloc; external pools must supply acquire_cached. */
int xip_pool_acquire_cached(xip_pool *pool, uint16_t reserve);
/* Idle slots whose storage is already allocated; cached RX can use these without malloc. */
uint16_t xip_pool_cached_available(const xip_pool *pool);
/* Owner-side preparation: warm up to 32 retained RX buffers, within keep policy. */
void xip_pool_prepare_rx(xip_pool *pool);
void xip_pool_retain(xip_pool *pool, int slot);
/* The last release frees the slot (and its storage, if more than `keep` idle slots have some). */
void xip_pool_release(xip_pool *pool, int slot);
uint32_t xip_pool_refs(const xip_pool *pool, int slot);
uint8_t *xip_pool_slot(xip_pool *pool, int slot);
uint16_t xip_pool_slot_size(const xip_pool *pool);
uint16_t xip_pool_available(const xip_pool *pool);
uint16_t xip_pool_slots(const xip_pool *pool);

/* A packet reference belongs to its caller, independent of module identity.
 * Handles name a live pool allocation; retaining adds a reference, transferring
 * passes that reference unchanged. The final release returns storage to the pool. */
typedef struct { int16_t slot; uint16_t start, len; } xip_packet_ref;
uint32_t xip_packet_handle(xip_pool *pool, int slot, uint16_t start, uint16_t len);
xip_packet_ref *xip_packet_get(xip_pool *pool, uint32_t handle);
int xip_packet_retain(xip_pool *pool, uint32_t handle);

/* ---- the managed heap of one module: the bytes it has taken and not given back ----
 * Only the balance is tracked, not the blocks: a module that stops with a balance above zero cannot be replaced live (the
 * host cannot know which memory is left), the device has to restart. */
typedef struct {
    uint32_t used, quota;         /* bytes */
} xip_mem;

void xip_mem_init(xip_mem *m, uint32_t quota);
void *xip_mem_alloc(xip_mem *m, size_t size);          /* NULL: over the quota or no heap; aligned for any type */
int xip_mem_free(xip_mem *m, void *ptr);                /* -1 for a pointer that is not from xip_mem_alloc (best effort) */
uint32_t xip_mem_used(const xip_mem *m);

#ifdef __cplusplus
}
#endif
