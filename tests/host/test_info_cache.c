/* Tests the production cache, including lifetime and exact CBOR validation. */
#include <assert.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include "xip.h"
#include "xip_cbor.h"
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
#define INFO_LOCK() pthread_mutex_lock(&mutex)
#define INFO_UNLOCK() pthread_mutex_unlock(&mutex)
static size_t available = 100000;
static size_t info_memory_available(void) { return available; }
#include "../../components/xip_runtime/src/info_cache.inc"
static const uint8_t status[] = {0xa1, 1, 0x82, 0xf5, 0xa0};
static bool found(const uint8_t *buf, size_t n, uint32_t comp, uint32_t method, uint64_t time)
{
    xip_cbor_r r, v; int64_t t;
    if (xip_cbor_get(buf, n, comp, &r)) return false;
    if (xip_cbor_get(r.p, r.len, method, &v)) return false;
    assert(!xip_cbor_get_int(v.p, v.len, 1, &t)); assert(t == (int64_t)time);
    assert(!xip_cbor_get(v.p, v.len, 2, &r));
    assert(r.len >= sizeof(status) && !memcmp(r.p, status, sizeof(status)));
    return true;
}
int main(void)
{
    uint8_t buf[256]; xip_cbor_w w;
    assert(info_publish(10, 44, 1, status, sizeof(status), 100) == 0);
    assert(info_publish(10, 44, 2, status, sizeof(status), 101) == 0);
    assert(info_publish(0, 50, 1, status, sizeof(status), 102) == 0);
    assert(info_publish(11, 44, 1, status, sizeof(status), 103) == 0);
    info_release(10);
    xip_cbor_init(&w, buf, sizeof(buf)); info_components(&w);
    int n = xip_cbor_done(&w); assert(n > 0);
    assert(found(buf, n, 44, 1, 103)); assert(!found(buf, n, 44, 2, 101)); assert(found(buf, n, 50, 1, 102));
    const uint8_t bad[][4] = {{0xa1, 1}, {0xa0, 0}, {0xa1, 1, 0x82, 0}, {0x81, 0xa0}};
    const size_t lengths[] = {2, 2, 4, 2};
    for (size_t i = 0; i < 4; i++) assert(info_publish(1, 44, 1, bad[i], lengths[i], 200) == XIP_ERR_ARG);
    assert(info_publish(1, -1, 1, status, sizeof(status), 200) == XIP_ERR_ARG);
    available = 32768;
    assert(info_publish(1, 44, 1, status, sizeof(status), 200) == XIP_ERR_NOMEM);
    xip_cbor_init(&w, buf, sizeof(buf)); info_components(&w);
    assert(found(buf, xip_cbor_done(&w), 44, 1, 103)); /* Failed replacement retained old entry. */
    available = 100000;
    xip_cbor_init(&w, buf, 1); info_components(&w); assert(xip_cbor_done(&w) == XIP_ERR_SIZE);
    assert(info_publish(0, 50, 1, NULL, 0, 0) == 0);
    info_release(11);
    xip_cbor_init(&w, buf, sizeof(buf)); info_components(&w); assert(xip_cbor_done(&w) == 1 && buf[0] == 0xa0);
    assert(!info_entries);
    puts("info cache: passed");
}
