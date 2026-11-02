/* SPDX-License-Identifier: Apache-2.0 */
/* The wasm3 port for this module: the interpreter's memory is the host's `mem_alloc` (every byte wasm3 and the app hold is a
 * tracked block, so a leaking run fails a test rather than draining the device heap), and the two debug hooks a build without
 * the profiler still references are empty. Nothing here is called by the module directly. */
#include <stddef.h>
#include "xip_module.h"
#include "xip_wasm3.h"

#ifndef XIP_CELL_ADDR
void *xip_w3_cell[4];
#endif

static const xip_host *w3_host(void)
{
    wasm3_state *st = (wasm3_state *)XIP_CELL[0];
    return st ? st->host : NULL;
}

void *xip_w3_alloc(size_t size)
{
    const xip_host *h = w3_host();
    if (!h || !size) return NULL;
    void *p = h->mem_alloc(h->user, size);
    if (p) { uint8_t *b = (uint8_t *)p; for (size_t i = 0; i < size; i++) b[i] = 0; } /* wasm3 expects calloc */
    return p;
}

void xip_w3_free(void *p)
{
    const xip_host *h = w3_host();
    if (p && h) h->mem_free(h->user, p);
}

/* the interpreter passes the old size; the new tail is zero, as its libc path does */
void *xip_w3_realloc(void *p, size_t size, size_t old)
{
    if (!p) return xip_w3_alloc(size);
    if (!size) { xip_w3_free(p); return NULL; }
    if (size == old) return p;
    void *n = xip_w3_alloc(size);
    if (!n) return NULL;                       /* a failed grow keeps the old block owned by the caller */
    xip_copy(n, p, old < size ? old : size);
    xip_w3_free(p);
    return n;
}

void m3_PrintProfilerInfo(void) {}
void log_opcode(void *vm, unsigned short opcode) { (void)vm; (void)opcode; }
