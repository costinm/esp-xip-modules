/* The pure part of the runtime's envelope handling (components/xip_runtime/src/xip_envelope.c): parsing, the answer head and the table of
 * requests that wait for their answer. */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "xip_cbor.h"
#include "xip_envelope.h"

static void test_parse(void)
{
    xip_envelope e;
    /* {1: 104, 2: 83, 3: 7, 5: {1: true}} */
    const uint8_t full[] = {0xa4, 0x01, 0x18, 104, 0x02, 0x18, 83, 0x03, 0x07, 0x05, 0xa1, 0x01, 0xf5};
    assert(xip_envelope_parse(full, sizeof full, &e) == XIP_ENV_REQUEST);
    assert(e.component == 104 && e.method == 83 && e.has_id && e.id == 7);
    assert(e.fields == full + 10 && e.fields_len == 3);
    /* fields in the middle of the map: the span is exactly the item */
    const uint8_t mid[] = {0xa4, 0x05, 0xa1, 0x01, 0x02, 0x01, 0x14, 0x02, 0x05, 0x03, 0x00};
    assert(xip_envelope_parse(mid, sizeof mid, &e) == XIP_ENV_REQUEST && e.fields == mid + 2 && e.fields_len == 3 && e.id == 0 && e.has_id);
    /* no id, no fields: a notification with an empty map */
    const uint8_t bare[] = {0xa2, 0x01, 0x14, 0x02, 0x05};
    assert(xip_envelope_parse(bare, sizeof bare, &e) == XIP_ENV_REQUEST && !e.has_id && e.fields_len == 1 && e.fields[0] == 0xa0);
    /* answers: a result or an error */
    const uint8_t ok[] = {0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x01, 0x06, 0xa0};
    const uint8_t bad[] = {0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x01, 0x07, 0x27};
    assert(xip_envelope_parse(ok, sizeof ok, &e) == XIP_ENV_ANSWER && e.component == 20);
    assert(xip_envelope_parse(bad, sizeof bad, &e) == XIP_ENV_ANSWER);
    /* invalid: no method, zero component, negative id, fields that are not a map, a truncated map */
    const uint8_t nometh[] = {0xa1, 0x01, 0x14};
    const uint8_t zero[] = {0xa2, 0x01, 0x00, 0x02, 0x05};
    const uint8_t negid[] = {0xa3, 0x01, 0x14, 0x02, 0x05, 0x03, 0x20};
    const uint8_t notmap[] = {0xa3, 0x01, 0x14, 0x02, 0x05, 0x05, 0x01};
    const uint8_t cut[] = {0xa3, 0x01, 0x14, 0x02, 0x05, 0x05, 0xa2, 0x01};
    assert(xip_envelope_parse(nometh, sizeof nometh, &e) == XIP_ENV_INVALID);
    assert(xip_envelope_parse(zero, sizeof zero, &e) == XIP_ENV_INVALID);
    assert(xip_envelope_parse(negid, sizeof negid, &e) == XIP_ENV_INVALID);
    assert(xip_envelope_parse(notmap, sizeof notmap, &e) == XIP_ENV_INVALID);
    assert(xip_envelope_parse(cut, sizeof cut, &e) == XIP_ENV_INVALID);
    /* not a map: a raw frame */
    const uint8_t text[] = "hello", empty[] = {0};
    assert(xip_envelope_parse(text, 5, &e) == XIP_ENV_NOT_MAP && xip_envelope_parse(empty, 0, &e) == XIP_ENV_NOT_MAP && xip_envelope_parse(NULL, 0, &e) == XIP_ENV_NOT_MAP);
    /* every truncation of a request is a clean answer */
    for (size_t n = 1; n < sizeof full; n++) {
        const xip_envelope_kind k = xip_envelope_parse(full, n, &e);
        assert(k == XIP_ENV_INVALID);
    }
}

static void test_head(void)
{
    uint8_t out[40];
    const uint8_t okhead[] = {0xa4, 0x01, 0x18, 104, 0x02, 0x18, 83, 0x03, 0x07, 0x06};
    size_t n = xip_envelope_head(out, sizeof out, 104, 83, 7, 0);
    assert(n == sizeof okhead && !memcmp(out, okhead, n));
    const uint8_t errhead[] = {0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x19, 0x01, 0x00, 0x07, 0x27};     /* component 20, method 5, id 256, error -8 */
    n = xip_envelope_head(out, sizeof out, 20, 5, 256, -8);
    assert(n == sizeof errhead && !memcmp(out, errhead, n));
    assert(xip_envelope_head(out, 5, 104, 83, 7, 0) == 0);
    n = xip_envelope_head(out, sizeof out, 1, 2, UINT64_MAX, -2147483647 - 1);
    assert(n > 0);
    /* a head and a result make an envelope the parser calls an answer */
    uint8_t whole[64];
    n = xip_envelope_head(whole, sizeof whole, 104, 83, 7, 0);
    whole[n++] = 0xa0;
    xip_envelope e;
    assert(xip_envelope_parse(whole, n, &e) == XIP_ENV_ANSWER && e.component == 104 && e.method == 83);
}

static void test_error_shape(void)
{
    int32_t rc = 0;
    const uint8_t e8[] = {0x27}, e300[] = {0x39, 0x01, 0x2b}, zero[] = {0x00}, map[] = {0xa0}, two[] = {0x27, 0x27};
    assert(xip_envelope_is_error(e8, 1, &rc) && rc == -8);
    assert(xip_envelope_is_error(e300, 3, &rc) && rc == -300);
    assert(!xip_envelope_is_error(zero, 1, &rc) && !xip_envelope_is_error(map, 1, &rc) && !xip_envelope_is_error(two, 2, &rc));
    assert(!xip_envelope_is_error(e300, 2, &rc) && !xip_envelope_is_error(NULL, 0, &rc));
}

static void test_table(void)
{
    xip_env_table t;
    memset(&t, 0, sizeof t);
    xip_env_slot s;
    assert(!xip_env_find(&t, 0x100, 4, 104, 83, &s));
    xip_env_add(&t, 0x100, 4, 104, 83, 1);
    xip_env_add(&t, 0x100, 4, 104, 83, 2);
    xip_env_add(&t, 0x100, 4, 104, 84, 9);
    assert(xip_env_find(&t, 0x100, 4, 104, 83, &s) && s.id == 2);      /* the newest */
    xip_env_done(&t, 0x100, 4, 104, 83, 2);
    assert(xip_env_find(&t, 0x100, 4, 104, 83, &s) && s.id == 1);
    assert(!xip_env_find(&t, 0x101, 4, 104, 83, &s) && !xip_env_find(&t, 0x100, 5, 104, 83, &s));
    xip_env_done(&t, 0x100, 4, 104, 83, 77);                              /* no such id: nothing happens */
    assert(t.n == 2);
    for (uint64_t i = 0; i < 20; i++) xip_env_add(&t, 1, 1, 1, 1, 100 + i);
    assert(t.n == XIP_ENV_PENDING);
    assert(xip_env_find(&t, 1, 1, 1, 1, &s) && s.id == 119);
    assert(!xip_env_find(&t, 0x100, 4, 104, 84, &s));                     /* the oldest were forgotten */
}

int main(void)
{
    test_parse();
    test_head();
    test_error_shape();
    test_table();
    puts("envelope: ok");
    return 0;
}
