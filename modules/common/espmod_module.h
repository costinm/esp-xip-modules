/* SPDX-License-Identifier: Apache-2.0 */
/* What a module author includes. A module is freestanding C: no libc, no globals, no static data
 * that is written (the image is read-only flash; tools/mkmod.py rejects .data and .bss). Everything it remembers
 * lives in the state block the host passes to every call (`--state BYTES` when packing); every service comes from
 * the host table. A module never waits: it reacts to calls and registers alarms or interrupts for what comes next
 * (abi/API.md, Model).
 *
 *   #include "espmod_module.h"
 *   ESPMOD_HANDLER(hello)(const espmod_host *host, void *state, const espmod_call *call)
 *   {
 *       if (call->component == ESPMOD_HOST_COMPONENT && call->method == ESPMOD_HOST_START) em_log(host, "hello");
 *       return ESPMOD_RET_DONE;
 *   }
 */
#pragma once

#include "espmod.h"
#include "espmod_cbor.h"

/* The flashed image exports `espmod_handle` (the linker script's entry point). The same source linked into
 * an application (-DESPMOD_LINKED) gets a distinct name, so several modules can live in one binary. */
#ifdef ESPMOD_LINKED
#define ESPMOD_HANDLER(name) int name##_handle
#else
#define ESPMOD_HANDLER(name) __attribute__((used)) int espmod_handle
#endif

/* ---- freestanding helpers (inline: nothing to link) ---- */
static inline size_t em_strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }

static inline void em_copy(void *dst, const void *src, size_t n)
{
    uint8_t *d = dst; const uint8_t *s = src;
    while (n--) *d++ = *s++;
}

static inline int em_equal(const void *a, const void *b, size_t n)
{
    const uint8_t *x = a, *y = b;
    while (n--) if (*x++ != *y++) return 0;
    return 1;
}

static inline int em_log(const espmod_host *h, const char *text)
{
    return h->log(h->user, (const uint8_t *)text, em_strlen(text));
}

static inline int em_emit_text(const espmod_host *h, uint16_t id, const char *text)
{
    return h->emit(h->user, id, ESPMOD_VALUE_TEXT, (const uint8_t *)text, em_strlen(text));
}

/* Decimal text of a signed number into buf (>= 12 bytes); returns its length. */
static inline size_t em_itoa(char *buf, long v)
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
static inline int em_starts(const uint8_t *req, size_t len, const char *w)
{
    size_t n = em_strlen(w);
    return len >= n && em_equal(req, w, n);
}
