/* SPDX-License-Identifier: Apache-2.0 */
/* The wasm3 module: runs a small WebAssembly app a caller sends, as an interpreter. The app is not a module of the
 * runtime - no 64 KiB alignment, no slot, no image: its bytes arrive in the request and are gone when the call returns,
 * and everything it uses is a tracked host `mem_alloc` block. Request method 1 `run` on the module's own component 51:
 *
 *   request  {1: bytes, 2: text?, 3: [int]?, 4: uint?}   the wasm binary; the export to call (default "run");
 *                                                       its integer arguments; the memory cap in bytes (default 16 KiB)
 *   response {1: int?, 2: text?, 3: uint}               the result (when the export returns one); what the app logged,
 *                                                       or the error - truncated at 512 bytes; a status:
 *                                                       0 ok, 1 not loadable (parse, link, no such export), 2 trapped, 3 bad request
 *
 * Method 2 `run_file` takes the same options with {1: name_bytes}, and loads
 * the program through the direct runtime files service before executing it.
 * The app imports from module `xip`:  log(ptr, len) -> i32   and   now_us() -> i64.
 * Memory pages of any size work: the binary may declare the custom page size proposal `(pagesize 1)`.
 * wasm3 lives in wasm3/ (vendored) and its port in port/. */
#include "xip_module.h"
#include "xip_wasm3.h"
#include "xip_native_libc.h"
#include "wasm3.h"

_Static_assert(sizeof(wasm3_state) <= 1696, "keep wasm3_state inside the declared state block");

#define WASM3_COMPONENT 51   /* the module's tag (modules/C-MODULES) */
#define WASM3_RUN       1    /* method 1: run */
#define WASM3_STACK     4096 /* bytes of the app's value stack */
#define WASM3_MEM_CAP   16384
#define WASM3_ARGS      4

static wasm3_state *w3_here(void) { return (wasm3_state *)XIP_CELL[0]; }

static void w3_out(wasm3_state *st, const char *s, size_t n)
{
    size_t room = WASM3_OUT_CAP - st->out.len;
    if (n > room) n = room;
    xip_copy(st->out.data + st->out.len, s, n);
    st->out.len = (uint16_t)(st->out.len + n);
}

static void w3_out_text(wasm3_state *st, const char *s) { w3_out(st, s, xip_strlen(s)); }

/* ---- what the app can call ---- */

/* the raw-function signature is fixed by wasm3; an import uses only some of it */
#define W3_UNUSED (void)runtime; (void)_ctx; (void)_sp; (void)_mem

m3ApiRawFunction(xip_import_log)
{
    W3_UNUSED;
    m3ApiReturnType(int32_t);
    m3ApiGetArgMem(const uint8_t *, text);
    m3ApiGetArg(int32_t, len);
    m3ApiCheckMem(text, len);
    wasm3_state *st = w3_here();
    if (st && len > 0) {
        w3_out(st, (const char *)text, (size_t)len);
        char line[96];
        size_t n = (size_t)len < sizeof line - 1 ? (size_t)len : sizeof line - 1;
        xip_copy(line, text, n);
        line[n] = 0;
        xip_log(st->host, line);
    }
    m3ApiReturn(0);
}

m3ApiRawFunction(xip_import_now_us)
{
    W3_UNUSED;
    m3ApiReturnType(int64_t);
    wasm3_state *st = w3_here();
    m3ApiReturn((int64_t)st->host->now_us(st->host->user));
}

/* ---- running one app ---- */

typedef struct {
    int has_result;
    int64_t result;
    int status;
} w3_run;

static void w3_fail(wasm3_state *st, w3_run *r, int status, const char *what)
{
    r->status = status;
    w3_out_text(st, what);
}

static void w3_execute(wasm3_state *st, w3_run *r, const uint8_t *wasm, size_t wasm_len, const char *name,
                       const int64_t *args, size_t argc, uint64_t cap)
{
    IM3Environment env = m3_NewEnvironment();
    if (!env) { w3_fail(st, r, 1, "no memory for the environment"); return; }
    IM3Runtime rt = m3_NewRuntime(env, WASM3_STACK, st);
    if (!rt) { w3_fail(st, r, 1, "no memory for the runtime"); m3_FreeEnvironment(env); return; }
    IM3Module mod = NULL;
    M3Result err = m3_ParseModule(env, &mod, wasm, (uint32_t)wasm_len);
    if (err) { w3_fail(st, r, 1, err); goto done; }
    /* the cap applies to the app's memory, whatever it declared or grows to; it is set before the load that allocates it */
    if ((err = m3_SetResourceLimit(rt, c_m3Limit_MemoryBytes, cap))) { m3_FreeModule(mod); w3_fail(st, r, 1, err); goto done; }
    if ((err = m3_LoadModule(rt, mod))) { w3_fail(st, r, 1, err); goto done; }   /* the runtime owns the module, loaded or not */
    m3_LinkRawFunction(mod, "xip", "log", "i(*i)", xip_import_log);
    m3_LinkRawFunction(mod, "xip", "now_us", "I()", xip_import_now_us);
    IM3Function fn;
    if ((err = m3_FindFunction(&fn, rt, name))) { w3_fail(st, r, 1, err); goto done; }
    if (m3_GetArgCount(fn) != argc) { w3_fail(st, r, 3, "wrong number of arguments"); goto done; }

    const void *argp[WASM3_ARGS];
    int32_t a32[WASM3_ARGS];
    int64_t a64[WASM3_ARGS];
    for (size_t i = 0; i < argc; i++) {
        switch (m3_GetArgType(fn, (uint32_t)i)) {
        case c_m3Type_i32: a32[i] = (int32_t)args[i]; argp[i] = &a32[i]; break;
        case c_m3Type_i64: a64[i] = args[i]; argp[i] = &a64[i]; break;
        default: w3_fail(st, r, 3, "only integer arguments"); goto done;
        }
    }
    if ((err = m3_Call(fn, (uint32_t)argc, argc ? argp : NULL))) { w3_fail(st, r, 2, err); goto done; }

    if (m3_GetRetCount(fn) == 1) {
        int32_t v32 = 0;
        int64_t v64 = 0;
        const void *retp[1];
        M3ValueType t = m3_GetRetType(fn, 0);
        retp[0] = t == c_m3Type_i64 ? (void *)&v64 : (void *)&v32;
        if (t != c_m3Type_i32 && t != c_m3Type_i64) { w3_fail(st, r, 3, "only integer results"); goto done; }
        if (!m3_GetResults(fn, 1, retp)) { r->has_result = 1; r->result = t == c_m3Type_i64 ? v64 : v32; }
    }
done:
    m3_FreeRuntime(rt);          /* frees the modules it owns, too */
    m3_FreeEnvironment(env);
}

static void w3_reply(const xip_host *host, wasm3_state *st, const xip_call *call, const xip_cbor_w *w)
{
    int n = xip_cbor_done(w);
    if (n < 0) { xip_log(host, "wasm3: answer too large"); return; }
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

static void w3_request(const xip_host *host, wasm3_state *st, const xip_call *call, const uint8_t *program, size_t program_len)
{
    w3_run r = { 0, 0, 3 };
    st->out.len = 0;
    xip_cbor_r field;
    xip_cbor_item item;
    const uint8_t *wasm = program;
    size_t wasm_len = program_len;
    if (!program && !xip_cbor_get(call->data, call->len, 1, &field) && !xip_cbor_next(&field, &item) && item.type == XIP_CBOR_BYTES) {
        wasm = item.data;
        wasm_len = item.n;
    }
    char name[32] = "run";
    if (!xip_cbor_get(call->data, call->len, 2, &field) && !xip_cbor_next(&field, &item) && item.type == XIP_CBOR_TEXT &&
        item.n && item.n < sizeof name) {
        xip_copy(name, item.data, item.n);
        name[item.n] = 0;
    }
    int64_t args[WASM3_ARGS];
    size_t argc = 0;
    int args_ok = 1;
    if (!xip_cbor_get(call->data, call->len, 3, &field) && !xip_cbor_next(&field, &item)) {
        if (item.type != XIP_CBOR_ARRAY || item.n > WASM3_ARGS) args_ok = 0;
        else for (; argc < item.n; argc++) {
            xip_cbor_item v;
            if (xip_cbor_next(&field, &v) || xip_cbor_as_int(&v, &args[argc])) { args_ok = 0; break; }
        }
    }
    int64_t cap = WASM3_MEM_CAP;
    if (!xip_cbor_get_int(call->data, call->len, 4, &cap) && cap < 0) args_ok = 0;
    if (wasm && args_ok) {
        r.status = 0;
        w3_execute(st, &r, wasm, wasm_len, name, args, argc, (uint64_t)cap);
    } else {
        w3_out_text(st, "bad request");
    }

    xip_cbor_w w;
    xip_cbor_init(&w, (uint8_t *)st->rsp.data, sizeof st->rsp.data);
    xip_cbor_map(&w, (size_t)(r.has_result ? 1 : 0) + (st->out.len ? 1 : 0) + 1);
    if (r.has_result) { xip_cbor_uint(&w, 1); xip_cbor_int(&w, r.result); }
    if (st->out.len)  { xip_cbor_uint(&w, 2); xip_cbor_text(&w, st->out.data, st->out.len); }
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, (uint64_t)r.status);
    w3_reply(host, st, call, &w);
    st->out.len = 0;
}

XIP_HANDLER(wasm3)(const xip_host *host, void *state, const xip_call *call)
{
    wasm3_state *st = (wasm3_state *)state;
    int rc=XIP_RET_CONTINUE;
    st->host=host;void *saved=XIP_CELL[0];XIP_CELL[0]=st;
    if(call->component==XIP_HOST_COMPONENT && call->method==XIP_HOST_START){
        xip_log(host,"hello from mod_wasm3");
    } else if(call->component==XIP_HOST_COMPONENT && call->method==XIP_HOST_STOP){
        xip_file_load_free(host,st->load);st->load=NULL;rc=XIP_RET_DONE;
    } else if(st->load && ((call->component==XIP_FILES_COMPONENT && call->method==XIP_FILES_GET && call->id==st->load->token) || (call->component==WASM3_COMPONENT && call->method==XIP_FILES_LOAD_TIMEOUT))){
        int result=call->component==WASM3_COMPONENT?XIP_ERR_TIMEOUT:xip_file_load_receive(host,st->load,call);
        if(result!=0){
            if(result<0)xip_file_error(host,&st->load->request,result);
            else w3_request(host,st,&st->load->request,st->load->data,st->load->size);
            xip_file_load_free(host,st->load);st->load=NULL;
        }
    } else if(call->component==WASM3_COMPONENT && call->method==2){
        int result=xip_file_load_start(host,call,&st->load);
        if(result<0)xip_file_error(host,call,result);
    } else if(call->component==WASM3_COMPONENT && call->method==WASM3_RUN){
        w3_request(host,st,call,NULL,0);
    }
    XIP_CELL[0]=saved;st->host=NULL;
    if(call->pkt)host->pkt_free(host->user,call->pkt);
    return rc;
}
