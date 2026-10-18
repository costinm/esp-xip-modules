/* SPDX-License-Identifier: Apache-2.0 */
/* components/xip_runtime (xip_pool.c): the slots, the reserve for the network, and the table of packets a module holds. */
#include <stdio.h>
#include <string.h>
#include <pthread.h>
#include "xip_pool.h"

static int failures;
#define CHECK(c) do { if (!(c)) { printf("FAIL line %d: %s\n", __LINE__, #c); failures++; } } while (0)

static size_t low_heap(void) { return 100; }

static uint8_t fake_mem[4 * 1204];
static unsigned fake_refs[4], fake_free_count = 4;
static int fake_acquire(void *c, uint16_t reserve) { (void)c; if (fake_free_count <= reserve) return -1; for (int i = 0; i < 4; i++) if (!fake_refs[i]) { fake_refs[i] = 1; fake_free_count--; return i; } return -1; }
static void fake_retain(void *c, int s) { (void)c; fake_refs[s]++; }
static void fake_release(void *c, int s) { (void)c; if (--fake_refs[s] == 0) fake_free_count++; }
static uint32_t fake_refs_of(void *c, int s) { (void)c; return fake_refs[s]; }
static uint8_t *fake_data(void *c, int s) { (void)c; return fake_mem + s * 1204; }
static uint16_t fake_size(void *c) { (void)c; return 1204; }
static uint16_t fake_slots(void *c) { (void)c; return 4; }
static uint16_t fake_avail(void *c) { (void)c; return (uint16_t)fake_free_count; }
static const xip_pool_ops fake_ops = {0, fake_acquire, fake_retain, fake_release, fake_refs_of, fake_data, fake_size, fake_slots, fake_avail};

static xip_pool *shared;
static int taken[2000];

static void *grab_release(void *arg)
{
    (void)arg;
    for (int i = 0; i < 20000; i++) {
        int s = xip_pool_acquire(shared, 2);
        if (s >= 0) { xip_pool_slot(shared, s)[0]++; xip_pool_release(shared, s); }
    }
    return NULL;
}

int main(void)
{
    xip_pool *p = xip_pool_create(8, 1204);
    CHECK(p && xip_pool_available(p) == 8 && xip_pool_slots(p) == 8 && xip_pool_slot_size(p) == 1204);

    /* slots, references */
    int a = xip_pool_acquire(p, 0), b = xip_pool_acquire(p, 0);
    CHECK(a >= 0 && b >= 0 && a != b && xip_pool_available(p) == 6);
    xip_pool_retain(p, a);
    xip_pool_release(p, a);
    CHECK(xip_pool_available(p) == 6 && xip_pool_refs(p, a) == 1);
    xip_pool_release(p, a);
    CHECK(xip_pool_available(p) == 7 && xip_pool_refs(p, a) == 0);
    xip_pool_release(p, a);                       /* a double release does nothing */
    CHECK(xip_pool_available(p) == 7);
    xip_pool_release(p, b);
    CHECK(xip_pool_available(p) == 8);

    /* the reserve: a module cannot take the last slots */
    int got[8], n = 0;
    while ((got[n] = xip_pool_acquire(p, 3)) >= 0) n++;
    CHECK(n == 5 && xip_pool_available(p) == 3);
    int net = xip_pool_acquire(p, 0);             /* the application still can */
    CHECK(net >= 0 && xip_pool_available(p) == 2);
    for (int i = 0; i < n; i++) xip_pool_release(p, got[i]);
    xip_pool_release(p, net);
    CHECK(xip_pool_available(p) == 8);
    /* storage is dynamic: none until a slot is used, given back when more than `keep` idle slots hold some */
    CHECK(xip_pool_allocated(p) == 2);                      /* the two kept idle of the eight that were used */
    int tt[5];
    for (int i = 0; i < 5; i++) tt[i] = xip_pool_acquire(p, 0);
    CHECK(xip_pool_allocated(p) == 5 && xip_pool_slot(p, tt[0]) && xip_pool_slot(p, tt[4]) && !xip_pool_slot(p, 8));
    for (int i = 0; i < 5; i++) xip_pool_release(p, tt[i]);
    CHECK(xip_pool_allocated(p) == 2 && xip_pool_available(p) == 8);
    xip_pool_set_policy(p, 0, NULL, 0);
    xip_pool_trim(p);
    CHECK(xip_pool_allocated(p) == 0);                      /* keep 0: everything goes back */

    /* no storage when the heap is short (a device's own reserve) */
    xip_pool_set_policy(p, 2, low_heap, 1000);
    CHECK(xip_pool_acquire(p, 0) == -1 && xip_pool_available(p) == 8);
    xip_pool_set_policy(p, 2, NULL, 0);

    /* a pool that belongs to someone else, behind the same calls */
    xip_pool *ext = xip_pool_external(&fake_ops);
    CHECK(ext && xip_pool_slot_size(ext) == 1204 && xip_pool_slots(ext) == 4 && xip_pool_available(ext) == 4);
    int xs = xip_pool_acquire(ext, 1);
    CHECK(xs == 0 && xip_pool_refs(ext, xs) == 1 && xip_pool_slot(ext, xs) == fake_mem && fake_free_count == 3);
    xip_pool_retain(ext, xs);
    xip_pool_release(ext, xs);
    CHECK(fake_refs[0] == 1);
    xip_pool_release(ext, xs);
    CHECK(fake_free_count == 4);
    xip_pool_destroy(ext);

    /* the held table: handles, generations, quota */
    xip_held h;
    xip_held_init(&h, 2);
    uint32_t x = xip_held_add(&h, 3, 144, 10, XIP_HELD_OWNED);
    uint32_t y = xip_held_add(&h, 4, 144, 0, XIP_HELD_OWNED);
    CHECK(x && y && x != y && !xip_held_add(&h, 5, 144, 0, XIP_HELD_OWNED));   /* quota 2 */
    uint32_t c = xip_held_add(&h, 6, 144, 5, XIP_HELD_BORROWED);                /* borrowed ones are not counted */
    CHECK(c && xip_held_get(&h, c)->slot == 6 && xip_held_get(&h, x)->len == 10);
    CHECK(xip_held_keep(&h, c) == -2);            /* keeping would pass the quota */
    xip_held_entry t = xip_held_take(&h, x);
    CHECK(t.slot == 3 && !xip_held_get(&h, x));  /* the handle is gone */
    CHECK(xip_held_take(&h, x).slot == -1);       /* taking twice finds nothing */
    CHECK(xip_held_keep(&h, c) == 0 && xip_held_get(&h, c)->state == XIP_HELD_OWNED);
    uint32_t z = xip_held_add(&h, 7, 144, 0, XIP_HELD_OWNED);   /* reuses x's place with a new generation */
    CHECK(z == 0);                                   /* y and c are the two */
    xip_held_take(&h, y);
    z = xip_held_add(&h, 7, 144, 0, XIP_HELD_OWNED);
    CHECK(z && z != x && z != y && !xip_held_get(&h, y) && !xip_held_get(&h, x));
    CHECK(!xip_held_get(&h, 0) && !xip_held_get(&h, 0xff) && !xip_held_get(&h, 0xffffff));
    CHECK(xip_held_count(&h, XIP_HELD_OWNED) == 2);

    /* two tasks hammering the pool never go below the reserve or lose a slot */
    shared = p;
    pthread_t t1, t2;
    pthread_create(&t1, NULL, grab_release, NULL);
    pthread_create(&t2, NULL, grab_release, NULL);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    CHECK(xip_pool_available(p) == 8);
    (void)taken;

    /* the managed heap: a byte balance and a quota */
    xip_mem m;
    xip_mem_init(&m, 1000);
    void *x1 = xip_mem_alloc(&m, 400), *x2 = xip_mem_alloc(&m, 400);
    CHECK(x1 && x2 && xip_mem_used(&m) == 800 && ((uintptr_t)x1 & 15) == 0);
    CHECK(!xip_mem_alloc(&m, 400));                 /* over the quota */
    CHECK(!xip_mem_alloc(&m, 0) && !xip_mem_alloc(&m, 100000));
    CHECK(xip_mem_free(&m, x1) == 0 && xip_mem_used(&m) == 400);
    CHECK(xip_mem_free(&m, NULL) == -1);
    void *x3 = xip_mem_alloc(&m, 600);              /* the freed bytes are usable again */
    CHECK(x3 && xip_mem_used(&m) == 1000);
    CHECK(xip_mem_free(&m, x2) == 0 && xip_mem_free(&m, x3) == 0 && xip_mem_used(&m) == 0);

    xip_pool_destroy(p);
    puts(failures ? "FAILED" : "pool ok");
    return failures != 0;
}
