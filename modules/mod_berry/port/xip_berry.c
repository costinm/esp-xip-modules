/* SPDX-License-Identifier: Apache-2.0 */
/* The Berry port for this module. Upstream ships its reference I/O in the REPL (be_repl.c) and in
 * default/be_port.c; a module has neither a console nor a filesystem, so this file provides the
 * pieces the compiled interpreter links against:
 *   - the allocator: every Berry block is a host mem_alloc block, so the runtime's balance is exact
 *     and a stopped VM that leaked fails the test rather than draining the device heap;
 *   - the console capture: print, syntax errors, and stack tracebacks all reach the interpreter's
 *     output through be_writebuffer, and be_writebuffer is ours to implement;
 *   - the file API and be_readstring: present so `file` and `input()` exist and fail cleanly.
 * berry_conf.h names these (BE_EXPLICIT_*); nothing here is called by the module directly. */
#include "xip_module.h"
#include "xip_berry.h"

/* Host build: one cell for the whole binary. The module was written for a runtime that serves one
 * module call at a time, and that is how a linked build keeps it: crates/berry-c holds a lock
 * around each call to xip_handle, so one call - one instance's cell - is live at a time, as on a
 * device. A packed image replaces XIP_CELL with an absolute address and compiles out this. */
#ifndef XIP_CELL_ADDR
void *xip_cell[4];
#endif

static berry_state *berry_here(void) { return (berry_state *)XIP_CELL[0]; }

/* ---- the allocator ---- */

/* One managed block per Berry allocation with a 16-byte head holding the payload size: without the
 * old size (the shim signature is libc realloc's) a grow allocates, copies, and frees. mem_alloc
 * blocks are aligned for any type, and so is the payload behind the head. */
#define BERRY_ALLOC_HEAD 16

void *xip_be_malloc(size_t size)
{
    berry_state *st = berry_here();
    if (!st || !st->host || !size) return NULL;
    void *block = st->host->mem_alloc(st->host->user, size + BERRY_ALLOC_HEAD);
    if (!block) return NULL;
    *(size_t *)block = size;
    return (char *)block + BERRY_ALLOC_HEAD;
}

void xip_be_free(void *ptr)
{
    if (!ptr) return;
    berry_state *st = berry_here();
    if (!st || !st->host) return;
    st->host->mem_free(st->host->user, (char *)ptr - BERRY_ALLOC_HEAD);
}

void *xip_be_realloc(void *ptr, size_t size)
{
    if (!ptr) return xip_be_malloc(size);
    if (!size) { xip_be_free(ptr); return NULL; }
    berry_state *st = berry_here();
    if (!st || !st->host) return NULL;
    char *block = (char *)ptr - BERRY_ALLOC_HEAD;
    size_t old = *(size_t *)block;
    void *grown = st->host->mem_alloc(st->host->user, size + BERRY_ALLOC_HEAD);
    if (!grown) return NULL; /* a failed grow keeps the old block owned by the caller, as realloc does */
    *(size_t *)grown = size;
    xip_copy((char *)grown + BERRY_ALLOC_HEAD, ptr, old < size ? old : size);
    st->host->mem_free(st->host->user, block);
    return (char *)grown + BERRY_ALLOC_HEAD;
}

/* A throw with no VM to catch it, or `exit()` outside any handler: the VM is beyond recovery. A
 * module cannot abort the firmware, so it takes an illegal instruction - on device the exception
 * handler reports it and the watchdog resets the slot, which is what an abort of one module means. */
void xip_be_abort(void)
{
    berry_state *st = berry_here();
    if (st && st->host) xip_log(st->host, "berry: fatal error outside the virtual machine");
    __builtin_trap();
    for (;;) {}
}

void xip_be_exit(int status)
{
    berry_state *st = berry_here();
    if (st && st->host) {
        char num[12];
        xip_itoa(num, status);
        xip_log(st->host, "berry: exit(");
        st->host->log(st->host->user, (const uint8_t *)num, xip_strlen(num));
        xip_log(st->host, ") outside the virtual machine");
    }
    xip_be_abort();
}

/* ---- the console ---- */

/* Everything the interpreter prints - `print`, "syntax error", a traceback - arrives here, in
 * pieces, while a call is in flight. The capture belongs to the request that caused it; when the
 * buffer is full the rest is dropped and `full` says so (the answer then carries what fits). */
void be_writebuffer(const char *buffer, size_t length)
{
    berry_state *st = berry_here();
    if (!st) return;
    berry_out *o = &st->out;
    while (length) {
        size_t room = BERRY_OUT_CAP - o->len;
        if (!room) { o->full = 1; return; }
        size_t take = length < room ? length : room;
        xip_copy(o->data + o->len, buffer, take);
        o->len = (uint16_t)(o->len + take);
        buffer += take;
        length -= take;
    }
}

/* input() reads from nowhere: this module has no terminal. NULL is the stream's end, and Berry's
 * input() returns an empty line for it. */
char *be_readstring(char *buffer, size_t size)
{
    (void)buffer; (void)size;
    return NULL;
}

/* ---- the file API ---- */

/* be_loadlibs registers the `file` class at VM start, so its functions must link; there is no
 * filesystem in a module, and opening anything fails. The class exists and every method reports
 * the error a Berry program expects. */
void *be_fopen(const char *filename, const char *modes) { (void)filename; (void)modes; return NULL; }
int be_fclose(void *hfile) { (void)hfile; return -1; }
size_t be_fwrite(void *hfile, const void *buffer, size_t length) { (void)hfile; (void)buffer; return length; }
size_t be_fread(void *hfile, void *buffer, size_t length) { (void)hfile; (void)buffer; (void)length; return 0; }
char *be_fgets(void *hfile, void *buffer, int size) { (void)hfile; (void)buffer; (void)size; return NULL; }
int be_fseek(void *hfile, long offset) { (void)hfile; (void)offset; return -1; }
long int be_ftell(void *hfile) { (void)hfile; return -1; }
long int be_fflush(void *hfile) { (void)hfile; return 0; }
size_t be_fsize(void *hfile) { (void)hfile; return 0; }

/* ---- the log flush ---- */

void xip_berry_flush(const xip_host *host, berry_state *st)
{
    berry_out *o = &st->out;
    size_t start = 0;
    while (start < o->len) {
        size_t end = start;
        while (end < o->len && o->data[end] != '\n') end++;
        host->log(host->user, (const uint8_t *)o->data + start, end - start);
        start = end + 1;
    }
    if (o->full) xip_log(host, "berry: output truncated");
    o->len = 0;
    o->full = 0;
}
