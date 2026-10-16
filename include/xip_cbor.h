/* SPDX-License-Identifier: Apache-2.0 */
/* A small CBOR (RFC 8949) encoder and decoder for modules and hosts: header-only, freestanding (no libc), nothing
 * to link. Requests and answers of a module are one CBOR map whose keys are the field tags of its API.md, as in
 * the ssh-mesh envelope. Supported: unsigned and negative integers up to 64 bits, byte and text strings, arrays,
 * maps, booleans and null; definite lengths only. Floats, tags and indefinite lengths are rejected by the
 * decoder (`xip_cbor_next` returns -1) rather than guessed at.
 *
 *   uint8_t buf[64];
 *   xip_cbor_w w;
 *   xip_cbor_init(&w, buf, sizeof(buf));
 *   xip_cbor_map(&w, 2);
 *   xip_cbor_uint(&w, 1); xip_cbor_int(&w, -68);          // field 1: rssi
 *   xip_cbor_uint(&w, 2); xip_cbor_text(&w, "ok", 2);     // field 2: text
 *   int n = xip_cbor_done(&w);                           // length, or XIP_ERR_SIZE if buf was too small
 *
 *   uint64_t tag;  xip_cbor_item it;  xip_cbor_r v;
 *   if (xip_cbor_get(req, len, 1, &v) == 0 && xip_cbor_next(&v, &it) == 0 && xip_cbor_as_int(&it, &x) == 0) ...
 */
#pragma once

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define XIP_CBOR_UINT 0
#define XIP_CBOR_NEGINT 1
#define XIP_CBOR_BYTES 2
#define XIP_CBOR_TEXT 3
#define XIP_CBOR_ARRAY 4
#define XIP_CBOR_MAP 5
#define XIP_CBOR_BOOL 6    /* `arg` is 0 or 1 */
#define XIP_CBOR_NULL 7    /* null and undefined */

/* ---- encoder: writes into a caller buffer; an overflow is remembered and reported by xip_cbor_done ---- */
typedef struct { uint8_t *p; size_t cap, len; int overflow; } xip_cbor_w;

static inline void xip_cbor_init(xip_cbor_w *w, uint8_t *buf, size_t cap)
{
    w->p = buf; w->cap = cap; w->len = 0; w->overflow = 0;
}

static inline void xip_cbor_put(xip_cbor_w *w, const void *data, size_t n)
{
    if (w->len + n > w->cap) { w->overflow = 1; return; }
    const uint8_t *s = data;
    for (size_t i = 0; i < n; i++) w->p[w->len + i] = s[i];
    w->len += n;
}

/* 32-bit word operations only: a module links no libgcc, and 64-bit shifts by a variable are library calls. */
static inline void xip_cbor_head(xip_cbor_w *w, uint8_t major, uint64_t arg)
{
    uint8_t h[9];
    size_t n = 1;
    const uint32_t hi = (uint32_t)(arg >> 32), lo = (uint32_t)arg;
    if (hi == 0 && lo < 24) h[0] = (uint8_t)(major << 5 | lo);
    else if (hi == 0 && lo <= 0xff) { h[0] = (uint8_t)(major << 5 | 24); h[1] = (uint8_t)lo; n = 2; }
    else if (hi == 0 && lo <= 0xffff) { h[0] = (uint8_t)(major << 5 | 25); h[1] = (uint8_t)(lo >> 8); h[2] = (uint8_t)lo; n = 3; }
    else if (hi == 0) {
        h[0] = (uint8_t)(major << 5 | 26);
        h[1] = (uint8_t)(lo >> 24); h[2] = (uint8_t)(lo >> 16); h[3] = (uint8_t)(lo >> 8); h[4] = (uint8_t)lo;
        n = 5;
    } else {
        h[0] = (uint8_t)(major << 5 | 27);
        h[1] = (uint8_t)(hi >> 24); h[2] = (uint8_t)(hi >> 16); h[3] = (uint8_t)(hi >> 8); h[4] = (uint8_t)hi;
        h[5] = (uint8_t)(lo >> 24); h[6] = (uint8_t)(lo >> 16); h[7] = (uint8_t)(lo >> 8); h[8] = (uint8_t)lo;
        n = 9;
    }
    xip_cbor_put(w, h, n);
}

static inline void xip_cbor_uint(xip_cbor_w *w, uint64_t v) { xip_cbor_head(w, 0, v); }
static inline void xip_cbor_int(xip_cbor_w *w, int64_t v)
{
    if (v >= 0) xip_cbor_head(w, 0, (uint64_t)v); else xip_cbor_head(w, 1, (uint64_t)(-(v + 1)));
}
static inline void xip_cbor_bytes(xip_cbor_w *w, const void *data, size_t n) { xip_cbor_head(w, 2, n); xip_cbor_put(w, data, n); }
static inline void xip_cbor_text(xip_cbor_w *w, const char *s, size_t n) { xip_cbor_head(w, 3, n); xip_cbor_put(w, s, n); }
static inline void xip_cbor_array(xip_cbor_w *w, size_t items) { xip_cbor_head(w, 4, items); }
static inline void xip_cbor_map(xip_cbor_w *w, size_t pairs) { xip_cbor_head(w, 5, pairs); }
static inline void xip_cbor_bool(xip_cbor_w *w, int v) { uint8_t b = v ? 0xf5 : 0xf4; xip_cbor_put(w, &b, 1); }
static inline void xip_cbor_null(xip_cbor_w *w) { uint8_t b = 0xf6; xip_cbor_put(w, &b, 1); }

/* The encoded length, or -8 (XIP_ERR_SIZE) if the buffer was too small. */
static inline int xip_cbor_done(const xip_cbor_w *w) { return w->overflow ? -8 : (int)w->len; }

/* ---- decoder ---- */
typedef struct { const uint8_t *p; size_t len; } xip_cbor_r;

typedef struct {
    uint8_t type;           /* XIP_CBOR_* */
    uint64_t arg;           /* UINT: the value. NEGINT: -1 - arg. BOOL: 0/1. */
    const uint8_t *data;    /* BYTES, TEXT: the content (points into the input) */
    size_t n;               /* BYTES, TEXT: length. ARRAY: items. MAP: pairs. */
} xip_cbor_item;

static inline void xip_cbor_reader(xip_cbor_r *r, const uint8_t *data, size_t len) { r->p = data; r->len = len; }

/* Read one item head and, for bytes and text, its content. For an array or a map only the head is consumed: the
 * caller reads `n` items (`2 * n` for a map) next. 0 on success, -1 if the input is malformed or unsupported. */
static inline int xip_cbor_next(xip_cbor_r *r, xip_cbor_item *it)
{
    if (r->len == 0) return -1;
    const uint8_t b = r->p[0];
    const uint8_t major = b >> 5, info = b & 31;
    size_t used = 1;
    uint64_t arg = info;
    if (major == 7) {
        r->p++; r->len--;
        it->data = 0; it->n = 0;
        if (info == 20 || info == 21) { it->type = XIP_CBOR_BOOL; it->arg = info == 21; return 0; }
        if (info == 22 || info == 23) { it->type = XIP_CBOR_NULL; it->arg = 0; return 0; }
        return -1;   /* floats, other simple values, break */
    }
    if (info >= 28) return -1;   /* indefinite lengths and reserved */
    if (info >= 24) {
        const size_t extra = (size_t)1 << (info - 24);   /* 1, 2, 4, 8 */
        if (info > 27 || r->len < 1 + extra) return -1;
        uint32_t hi = 0, lo = 0;
        for (size_t i = 0; i < extra; i++) { hi = hi << 8 | lo >> 24; lo = lo << 8 | r->p[1 + i]; }
        arg = (uint64_t)hi << 32 | lo;
        used += extra;
    }
    if (major == 6) return -1;   /* tags */
    r->p += used; r->len -= used;
    it->type = major; it->arg = arg; it->data = 0; it->n = 0;
    if (major == 2 || major == 3) {
        if (arg > r->len) return -1;
        it->data = r->p; it->n = (size_t)arg;
        r->p += arg; r->len -= (size_t)arg;
    } else if (major == 4 || major == 5) {
        if (arg > r->len) return -1;   /* every item takes at least a byte: rejects absurd counts */
        it->n = (size_t)arg;
    }
    return 0;
}

/* Step over one whole item, including nested arrays and maps. */
static inline int xip_cbor_skip(xip_cbor_r *r)
{
    size_t pending = 1;
    while (pending) {
        xip_cbor_item it;
        if (xip_cbor_next(r, &it)) return -1;
        pending--;
        if (it.type == XIP_CBOR_ARRAY) pending += it.n;
        else if (it.type == XIP_CBOR_MAP) pending += 2 * it.n;
    }
    return 0;
}

/* The integer value of a UINT or NEGINT item; -1 if it is another type or does not fit in 64 bits signed. */
static inline int xip_cbor_as_int(const xip_cbor_item *it, int64_t *out)
{
    if (it->type == XIP_CBOR_UINT && it->arg <= 0x7fffffffffffffffull) { *out = (int64_t)it->arg; return 0; }
    if (it->type == XIP_CBOR_NEGINT && it->arg <= 0x7fffffffffffffffull) { *out = -1 - (int64_t)it->arg; return 0; }
    return -1;
}

/* In a CBOR map `data`, find the value of the unsigned-integer key `key` (a field tag). 0: found, `value` is
 * positioned at it; 1: no such key; -1: not a well-formed map. Keys of another type are skipped. */
static inline int xip_cbor_get(const uint8_t *data, size_t len, uint64_t key, xip_cbor_r *value)
{
    xip_cbor_r r;
    xip_cbor_item it;
    xip_cbor_reader(&r, data, len);
    if (xip_cbor_next(&r, &it) || it.type != XIP_CBOR_MAP) return -1;
    for (size_t i = 0; i < it.n; i++) {
        xip_cbor_item k;
        if (xip_cbor_next(&r, &k)) return -1;
        if (k.type == XIP_CBOR_ARRAY || k.type == XIP_CBOR_MAP) { r.p -= 0; return -1; }   /* only scalar keys */
        if (k.type == XIP_CBOR_UINT && k.arg == key) { *value = r; return 0; }
        if (xip_cbor_skip(&r)) return -1;
    }
    return 1;
}

/* Shorthands for the common fields. Return 0 when the field is present and of that type, 1 when it is absent, -1
 * when it is present but of another type or the map is malformed. */
static inline int xip_cbor_get_int(const uint8_t *data, size_t len, uint64_t key, int64_t *out)
{
    xip_cbor_r v; xip_cbor_item it;
    int rc = xip_cbor_get(data, len, key, &v);
    if (rc) return rc;
    return xip_cbor_next(&v, &it) || xip_cbor_as_int(&it, out) ? -1 : 0;
}

static inline int xip_cbor_get_str(const uint8_t *data, size_t len, uint64_t key, uint8_t type, const uint8_t **s, size_t *n)
{
    xip_cbor_r v; xip_cbor_item it;
    int rc = xip_cbor_get(data, len, key, &v);
    if (rc) return rc;
    if (xip_cbor_next(&v, &it) || it.type != type) return -1;
    *s = it.data; *n = it.n;
    return 0;
}

#ifdef __cplusplus
}
#endif
