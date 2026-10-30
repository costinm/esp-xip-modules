/* SPDX-License-Identifier: Apache-2.0 */
/* The Berry language runtime module: a resident interpreter evaluated through the module ABI.
 *
 * Start keeps the module resident and makes one Berry VM (host mem_alloc blocks only). A start
 * request is echoed as event 1 as before, and a start request beginning "run:" is also executed as
 * Berry source with its output logged. Request method 1 `eval` on the module's own component 50 runs
 * one piece of source and answers:
 *
 *   request  {1: text}                      Berry source
 *   response {1: text?, 2: text?, 3: uint}  result text (nil results omitted), what the
 *                                           interpreter wrote (print, error, traceback) - truncated
 *                                           at 512 bytes - and a status: 0 ok, 1 compile error,
 *                                           2 runtime error, 3 bad request, 4 no interpreter
 *
 * A bare expression answers its value: like the Berry REPL, the source is first tried as
 * `return (source)`, and when that will not compile, the source runs as written.
 *
 * Stop deletes the VM; every block it held goes back with it, or the runtime reports the leak.
 * The interpreter lives in berry/ (vendored) and its port in port/. */
#include "xip_module.h"
#include "xip_berry.h"
#include "berry.h"

_Static_assert(sizeof(berry_state) <= 1696, "keep berry_state inside the declared state block");

#define BERRY_COMPONENT 50   // the module's tag (modules/SLOTS; what a request carries)
#define BERRY_EVAL      1    // method 1: eval

/* Compile source the way Berry's own REPL does: try `return (src)` first, so a bare expression
 * evaluates to a value (the language itself never returns one from a script chunk), and fall back
 * to the source as written when that will not compile - `print(1)`, `x = 7`, `if` all take the
 * fallback. On success the compiled function is left on the stack, on failure the exception is.
 * The name is kept for both passes so an error still says `request:1:`. */
static int berry_compile(bvm *vm, const char *name, const char *src, size_t len)
{
    int res;
    int idx;
    be_pushnstring(vm, "return (", 8);         /* build the wrapper on the VM stack, the source */
    be_pushnstring(vm, src, len);              /* need not be NUL terminated, so it goes by length */
    be_strconcat(vm, -2);
    be_pop(vm, 1);
    be_pushnstring(vm, ")", 1);
    be_strconcat(vm, -2);
    be_pop(vm, 1);
    idx = be_absindex(vm, -1);
    res = be_loadbuffer(vm, name, be_tostring(vm, -1), len + 9);
    be_remove(vm, idx);
    if (!res) return 0;
    be_pop(vm, 2);                             /* this attempt's error is dropped: the fallback's is reported */
    return be_loadbuffer(vm, name, src, len);
}

/* Run one piece of source on the VM. The interpreter's own writing (print, errors) lands in
 * st->out; the return value of the script, converted to text, is returned - valid until the next
 * Berry allocation, so the caller copies it into the answer first. */
static const char *berry_run(berry_state *st, const char *name, const char *src, size_t len, int *status)
{
    st->out.len = 0;
    st->out.full = 0;
    bvm *vm = (bvm *)st->vm;
    *status = 0;
    if (berry_compile(vm, name, src, len)) {   /* compile: leaves the function, or the exception, on the stack */
        *status = 1;
        be_dumpexcept(vm);                     /* writes the error into the capture, pops it */
        return NULL;
    }
    if (be_pcall(vm, 0)) {                     /* run */
        *status = 2;
        be_dumpexcept(vm);
        return NULL;
    }
    if (be_isnil(vm, -1)) { be_pop(vm, 1); return NULL; }
    const char *result = be_tostring(vm, -1);
    be_pop(vm, 1);
    return result;
}

/* Answer a request with the CBOR built in the state block (the answer is at most PKT_MAX bytes). */
static void berry_reply(const xip_host *host, berry_state *st, const xip_call *call, const xip_cbor_w *w)
{
    int n = xip_cbor_done(w);
    if (n < 0) { xip_log(host, "berry: answer too large"); return; }
    uint32_t pkt = 0;
    if (host->pkt_alloc(host->user, &pkt)) return;
    uint8_t *window;
    size_t room;
    if (host->pkt_buf(host->user, pkt, &window, &room) || (size_t)n > room) {
        host->pkt_free(host->user, pkt);
        return;
    }
    xip_copy(window, st->rsp.data, (size_t)n);
    host->send(host->user, pkt, (size_t)n, call->from, call->component, call->method, call->id);
}

static void berry_eval(const xip_host *host, berry_state *st, const xip_call *call)
{
    const char *result = NULL;
    int status = 3;
    xip_cbor_r field;
    xip_cbor_item item;
    if (!st->vm) status = 4;
    else if (!xip_cbor_get(call->data, call->len, 1, &field) &&
             !xip_cbor_next(&field, &item) && item.type == XIP_CBOR_TEXT)
        result = berry_run(st, "request", (const char *)item.data, item.n, &status);

    size_t result_len = result ? xip_strlen(result) : 0;
    if (result_len > BERRY_OUT_CAP) result_len = BERRY_OUT_CAP; /* a huge string result is shown truncated */
    int has_result = result && result_len;
    int has_out = st->out.len != 0;
    xip_cbor_w w;
    xip_cbor_init(&w, (uint8_t *)st->rsp.data, sizeof st->rsp.data);
    xip_cbor_map(&w, (size_t)(has_result ? 1 : 0) + (has_out ? 1 : 0) + 1);
    if (has_result) { xip_cbor_uint(&w, 1); xip_cbor_text(&w, result, result_len); }
    if (has_out)    { xip_cbor_uint(&w, 2); xip_cbor_text(&w, st->out.data, st->out.len); }
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, (uint64_t)status);
    berry_reply(host, st, call, &w);
    st->out.len = 0;
    st->out.full = 0;
}

static int berry_handle(const xip_host *host, berry_state *st, const xip_call *call)
{
    if (call->component == XIP_HOST_COMPONENT && call->method == XIP_HOST_START) {
        xip_log(host, "hello from mod_berry");
        st->vm = be_vm_new();
        if (!st->vm) xip_log(host, "berry: out of memory for the interpreter");
        if (call->len && xip_starts(call->data, call->len, "run:")) {
            if (st->vm) {
                int status;
                const char *result = berry_run(st, "start", (const char *)call->data + 4, call->len - 4, &status);
                xip_berry_flush(host, st);                 /* what the script printed, line by line */
                if (result) { xip_log(host, "=> "); xip_log(host, result); }
                if (status) xip_log(host, "berry: start script failed");
            }
        } else if (call->len) {
            xip_notify(host, 1, call->data, call->len);    /* echo the start request as event 1 */
        }
        return XIP_RET_CONTINUE;                           /* stay resident: the interpreter serves requests */
    }
    if (call->component == XIP_HOST_COMPONENT && call->method == XIP_HOST_STOP) {
        if (st->vm) { be_vm_delete(st->vm); st->vm = NULL; } /* every Berry block comes back now or the stop leaks */
        return XIP_RET_DONE;
    }
    if (call->component == BERRY_COMPONENT && call->method == BERRY_EVAL) {
        berry_eval(host, st, call);
        return XIP_RET_CONTINUE;
    }
    return XIP_RET_CONTINUE;
}

XIP_HANDLER(berry)(const xip_host *host, void *state, const xip_call *call)
{
    berry_state *st = (berry_state *)state;
    st->host = host;                     /* the allocator and the capture run through this call's host */
    void *saved = XIP_CELL[0];
    XIP_CELL[0] = st;
    int rc = berry_handle(host, st, call);
    XIP_CELL[0] = saved;
    st->host = NULL;                     /* nothing may allocate for this instance outside a call */
    return rc;
}
