/* SPDX-License-Identifier: Apache-2.0 */
#include <string.h>
#include "xip_cbor.h"
#include "xip_envelope.h"

static const uint8_t empty_map[1] = {0xa0};

xip_envelope_kind xip_envelope_parse(const uint8_t *cbor, size_t len, xip_envelope *e)
{
    memset(e, 0, sizeof(*e));
    e->fields = empty_map; e->fields_len = 1;
    if (!cbor || !len || (cbor[0] >> 5) != 5) return XIP_ENV_NOT_MAP;
    xip_cbor_r v;
    xip_cbor_item it;
    const bool answer = xip_cbor_get(cbor, len, 6, &v) == 0 || xip_cbor_get(cbor, len, 7, &v) == 0;
    int64_t c, m, id = 0;
    if (xip_cbor_get_int(cbor, len, 1, &c) != 0 || xip_cbor_get_int(cbor, len, 2, &m) != 0) return answer ? XIP_ENV_ANSWER : XIP_ENV_INVALID;
    if (c <= 0 || c > INT32_MAX || m <= 0 || m > INT32_MAX) return XIP_ENV_INVALID;
    e->component = (int32_t)c; e->method = (int32_t)m;
    if (answer) return XIP_ENV_ANSWER;
    const int rc = xip_cbor_get_int(cbor, len, 3, &id);
    if (rc < 0 || (rc == 0 && id < 0)) return XIP_ENV_INVALID;
    e->has_id = rc == 0; e->id = rc == 0 ? (uint64_t)id : 0;
    const int f = xip_cbor_get(cbor, len, 5, &v);
    if (f < 0) return XIP_ENV_INVALID;
    if (f == 0) {
        xip_cbor_r probe = v;
        if (xip_cbor_next(&probe, &it) || it.type != XIP_CBOR_MAP) return XIP_ENV_INVALID;
        xip_cbor_r span = v;
        if (xip_cbor_skip(&span)) return XIP_ENV_INVALID;
        e->fields = v.p; e->fields_len = v.len - span.len;
    }
    return XIP_ENV_REQUEST;
}

size_t xip_envelope_head(uint8_t *out, size_t cap, int32_t component, int32_t method, uint64_t id, int32_t error)
{
    xip_cbor_w w;
    xip_cbor_init(&w, out, cap);
    xip_cbor_map(&w, 4);
    xip_cbor_uint(&w, 1); xip_cbor_int(&w, component);
    xip_cbor_uint(&w, 2); xip_cbor_int(&w, method);
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, id);
    if (error < 0) { xip_cbor_uint(&w, 7); xip_cbor_int(&w, error); } else xip_cbor_uint(&w, 6);
    const int n = xip_cbor_done(&w);
    return n < 0 ? 0 : (size_t)n;
}

bool xip_envelope_is_error(const uint8_t *p, size_t len, int32_t *error)
{
    if (!p || !len || (p[0] >> 5) != 1) return false;
    xip_cbor_r r;
    xip_cbor_item it;
    int64_t v;
    xip_cbor_reader(&r, p, len);
    if (xip_cbor_next(&r, &it) || r.len != 0 || xip_cbor_as_int(&it, &v) || v >= 0 || v < INT32_MIN) return false;
    *error = (int32_t)v;
    return true;
}

void xip_env_add(xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, uint64_t id)
{
    if (t->n == XIP_ENV_PENDING) {
        memmove(&t->s[0], &t->s[1], (XIP_ENV_PENDING - 1) * sizeof(t->s[0]));
        t->n--;
    }
    t->s[t->n++] = (xip_env_slot){.dst = dst, .token = token, .id = id, .component = component, .method = method};
}

static int env_at(const xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, bool by_id, uint64_t id)
{
    for (int i = (int)t->n - 1; i >= 0; i--) {
        const xip_env_slot *s = &t->s[i];
        if (s->dst == dst && s->token == token && s->component == component && s->method == method && (!by_id || s->id == id)) return i;
    }
    return -1;
}

bool xip_env_find(const xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, xip_env_slot *out)
{
    const int i = env_at(t, dst, token, component, method, false, 0);
    if (i < 0) return false;
    *out = t->s[i];
    return true;
}

void xip_env_done(xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, uint64_t id)
{
    const int i = env_at(t, dst, token, component, method, true, id);
    if (i < 0) return;
    memmove(&t->s[i], &t->s[i + 1], (size_t)(t->n - i - 1) * sizeof(t->s[0]));
    t->n--;
}
