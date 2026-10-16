/* SPDX-License-Identifier: Apache-2.0 */
/* What a module author includes. A module is freestanding C: no libc, no globals, no static data
 * that is written (the image is read-only flash; tools/mkmod.py rejects .data and .bss). Everything it remembers
 * lives in the state block the host passes to every call (`--state BYTES` when packing); every service comes from
 * the host table. A module never waits: it reacts to calls and registers alarms or interrupts for what comes next
 * (abi/API.md, Model).
 *
 *   #include "xip_module.h"
 *   XIP_HANDLER(hello)(const xip_host *host, void *state, const xip_call *call)
 *   {
 *       if (call->component == XIP_HOST_COMPONENT && call->method == XIP_HOST_START) xip_log(host, "hello");
 *       return XIP_RET_DONE;
 *   }
 */
#pragma once

#include "xip.h"
#include "xip_cbor.h"

/* The flashed image exports `xip_handle` (the linker script's entry point). The same source linked into
 * an application (-DXIP_LINKED) gets a distinct name, so several modules can live in one binary. */
#ifdef XIP_LINKED
#define XIP_HANDLER(name) int name##_handle
#else
#define XIP_HANDLER(name) __attribute__((used)) int xip_handle
#endif

/* ---- freestanding helpers (inline: nothing to link) ---- */
static inline size_t xip_strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }

static inline void xip_copy(void *dst, const void *src, size_t n)
{
    uint8_t *d = dst; const uint8_t *s = src;
    while (n--) *d++ = *s++;
}

static inline int xip_equal(const void *a, const void *b, size_t n)
{
    const uint8_t *x = a, *y = b;
    while (n--) if (*x++ != *y++) return 0;
    return 1;
}

static inline int xip_log(const xip_host *h, const char *text)
{
    return h->log(h->user, (const uint8_t *)text, xip_strlen(text));
}

static inline int xip_emit_text(const xip_host *h, uint16_t id, const char *text)
{
    return h->emit(h->user, id, XIP_VALUE_TEXT, (const uint8_t *)text, xip_strlen(text));
}

/* Decimal text of a signed number into buf (>= 12 bytes); returns its length. */
static inline size_t xip_itoa(char *buf, long v)
{
    char tmp[12];
    size_t n = 0, len = 0;
    unsigned long u = v < 0 ? 0ul - (unsigned long)v : (unsigned long)v;
    do { tmp[n++] = (char)('0' + u % 10); u /= 10; } while (u);
    if (v < 0) buf[len++] = '-';
    while (n) buf[len++] = tmp[--n];
    buf[len] = 0;
    return len;
}

/* True when the request starts with the word `w`. */
static inline int xip_starts(const uint8_t *req, size_t len, const char *w)
{
    size_t n = xip_strlen(w);
    return len >= n && xip_equal(req, w, n);
}
