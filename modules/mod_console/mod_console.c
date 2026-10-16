/* SPDX-License-Identifier: Apache-2.0 */
/* SPDX-License-Identifier: Apache-2.0 */
/* mod_console: the text interface of a device (API.md in this directory), and the only place text is made or read.
 *
 * It owns the serial port through the host (serial_open): every line typed arrives as a request, and what the handler
 * answers is printed. A line is `status`, `start NAME [TEXT]`, `stop NAME`, `events`, `reset`, `wifi ...`,
 * `help`, or `component.method key=value ...`: the last turns into the request payload of the module `component`
 * (tagged CBOR, named by catalog.h, generated from the modules' API.md files), calls it with the host's call(), and
 * turns its answer back into text. The host moves bytes and structured records; it formats nothing. */
/* mkmod: --state 8 --stack 6144 --features SERIAL,PACKETS,MEMORY --autostart */
#include "xip_module.h"
#include "catalog.h"

#define C_CONSOLE 20
#define M_PARSE 1
#define M_RENDER 2
#define MAX_TOKENS 24
#define OUT_MAX 1000
#define RESP_MAX 1000

/* ---- catalog lookups ---- */
static const char *cname(unsigned off) { return (const char *)&cat_pool[off]; }

static int same(const char *a, const char *b, size_t n)
{
    size_t i = 0;
    for (; i < n; i++) if (a[i] != b[i]) return 0;
    return b[i] == 0;
}

static const cat_comp *find_comp(const char *s, size_t n, unsigned long id, int is_id)
{
    for (int i = 0; i < CAT_COMPS; i++)
        if ((cat_comps[i].name && same(s, cname(cat_comps[i].name), n)) || (is_id && cat_comps[i].id == id)) return &cat_comps[i];
    return 0;
}

static const cat_method *find_method(const cat_comp *c, const char *s, size_t n, unsigned long id, int is_id)
{
    for (unsigned i = 0; i < c->count; i++) {
        const cat_method *m = &cat_methods[c->first + i];
        if (same(s, cname(m->name), n) || (is_id && m->id == id)) return m;
    }
    return 0;
}

static const cat_method *method_of(unsigned comp_id, unsigned method_id)
{
    for (int i = 0; i < CAT_COMPS; i++) {
        if (cat_comps[i].id != comp_id) continue;
        for (unsigned k = 0; k < cat_comps[i].count; k++)
            if (cat_methods[cat_comps[i].first + k].id == method_id) return &cat_methods[cat_comps[i].first + k];
    }
    return 0;
}

static const cat_field *find_field(const cat_field *list, unsigned count, const char *s, size_t n)
{
    for (unsigned i = 0; i < count; i++) if (list[i].name && same(s, cname(list[i].name), n)) return &list[i];
    return 0;
}

static const cat_field *field_by_tag(const cat_field *list, unsigned count, unsigned tag)
{
    for (unsigned i = 0; i < count; i++) if (list[i].tag == tag) return &list[i];
    return 0;
}

/* ---- text helpers ---- */
static int digits(const char *s, size_t n, uint64_t *v)
{
    if (!n) return 0;
    uint64_t x = 0;
    for (size_t i = 0; i < n; i++) {
        if (s[i] < '0' || s[i] > '9') return 0;
        x = (x << 3) + (x << 1) + (uint64_t)(s[i] - '0');
    }
    *v = x;
    return 1;
}

static int hexval(char c) { return c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1; }

/* The blocks one call took with mem_alloc, given back together at the end of the handler: this module's own bookkeeping, the
 * host has no notion of per-call memory. */
typedef struct { void *p[16]; int n; } mem_t;
static void *take(const xip_host *h, mem_t *mem, size_t size)
{
    void *b = mem->n < 16 ? h->mem_alloc(h->user, size) : 0;
    if (b) mem->p[mem->n++] = b;
    return b;
}
static void give_back(const xip_host *h, mem_t *mem) { while (mem->n) (void)h->mem_free(h->user, mem->p[--mem->n]); }

/* Output buffer for text. */
typedef struct { char *p; size_t cap, len; } out_t;
static void put(out_t *o, const char *s, size_t n) { for (size_t i = 0; i < n && o->len + 1 < o->cap; i++) o->p[o->len++] = s[i]; }
static void puts_(out_t *o, const char *s) { put(o, s, xip_strlen(s)); }
/* v / 10 and v % 10 with 32-bit operations only: a module links no libgcc (no __udivdi3) */
static uint32_t divmod10(uint64_t *v)
{
    uint32_t hi = (uint32_t)(*v >> 32), lo = (uint32_t)*v;
    uint32_t qh = hi / 10, r = hi % 10;
    uint32_t cur = (r << 16) | (lo >> 16);
    uint32_t q1 = cur / 10;
    r = cur % 10;
    cur = (r << 16) | (lo & 0xffff);
    uint32_t q0 = cur / 10;
    r = cur % 10;
    *v = ((uint64_t)qh << 32) | ((uint64_t)q1 << 16) | q0;
    return r;
}
static void putu(out_t *o, uint64_t v) { char d[24]; int k = 0; do { d[k++] = (char)('0' + divmod10(&v)); } while (v); while (k) put(o, &d[--k], 1); }
static void putn(out_t *o, int64_t v) { char b[24]; size_t n = xip_itoa(b, (long)v); put(o, b, n); }

/* A JSON-like dump of the host's CBOR: numeric tags stay numeric. */
static int info_json(out_t *o, xip_cbor_r *r, unsigned depth)
{
    xip_cbor_item it;
    if (depth > 32 || xip_cbor_next(r, &it)) return -1;
    if (it.type == XIP_CBOR_MAP || it.type == XIP_CBOR_ARRAY) {
        int map = it.type == XIP_CBOR_MAP;
        put(o, map ? "{" : "[", 1);
        for (size_t i = 0; i < it.n; i++) {
            if (i) put(o, ",", 1);
            if (map) { if (info_json(o, r, depth + 1)) return -1; put(o, ":", 1); }
            if (info_json(o, r, depth + 1)) return -1;
        }
        put(o, map ? "}" : "]", 1);
    } else if (it.type == XIP_CBOR_TEXT || it.type == XIP_CBOR_BYTES) {
        const char *hex = "0123456789abcdef";
        put(o, "\"", 1);
        if (it.type == XIP_CBOR_BYTES) puts_(o, "hex:");
        for (size_t i = 0; i < it.n; i++) {
            uint8_t c = it.data[i];
            if (it.type == XIP_CBOR_BYTES) {
                char b[2] = {hex[c >> 4], hex[c & 15]}; put(o, b, 2);
            } else if (c == '"' || c == '\\') {
                put(o, "\\", 1); put(o, (const char *)&c, 1);
            } else if (c < 32) {
                char b[6] = {'\\', 'u', '0', '0', hex[c >> 4], hex[c & 15]}; put(o, b, 6);
            } else put(o, (const char *)&c, 1);
        }
        put(o, "\"", 1);
    } else if (it.type == XIP_CBOR_UINT) putu(o, it.arg);
    else if (it.type == XIP_CBOR_NEGINT) {
        put(o, "-", 1);
        if (it.arg == UINT64_MAX) puts_(o, "18446744073709551616"); else putu(o, it.arg + 1);
    } else if (it.type == XIP_CBOR_BOOL) puts_(o, it.arg ? "true" : "false");
    else if (it.type == XIP_CBOR_NULL) puts_(o, "null");
    else return -1;
    return 0;
}
static void cmd_status(const xip_host *h, out_t *o)
{
    if (!XIP_HOST_HAS(h, device_info)) { puts_(o, "error: this host has no device info\n"); return; }
    size_t cap = RESP_MAX;
    uint8_t *buf = 0;
    int n;
    for (;;) {
        buf = h->mem_alloc(h->user, cap);
        if (!buf) { puts_(o, "error: out of memory\n"); return; }
        n = h->device_info(h->user, buf, cap);
        if (n != XIP_ERR_SIZE) break;
        h->mem_free(h->user, buf);
        if (cap > SIZE_MAX / 2) { puts_(o, "error: status too large\n"); return; }
        cap *= 2;
    }
    if (n < 0 || (size_t)n > (SIZE_MAX - 2) / 6) {
        h->mem_free(h->user, buf); puts_(o, "error: device info unavailable\n"); return;
    }
    size_t text_cap = (size_t)n * 6 + 2;
    char *text = h->mem_alloc(h->user, text_cap);
    if (!text) { h->mem_free(h->user, buf); puts_(o, "error: out of memory\n"); return; }
    out_t dump = {text, text_cap, 0};
    xip_cbor_r r; xip_cbor_reader(&r, buf, (size_t)n);
    if (info_json(&dump, &r, 0) || r.len) puts_(o, "error: malformed device info\n");
    else {
        put(&dump, "\n", 1);
        if (h->features & XIP_FEAT_SERIAL) (void)h->serial_write(h->user, (const uint8_t *)text, dump.len);
    }
    h->mem_free(h->user, text); h->mem_free(h->user, buf);
}

/* ---- parse ---- */
typedef struct { char *s; size_t n; } tok_t;

static int tokenize(char *line, size_t len, tok_t *t)
{
    int count = 0;
    size_t i = 0;
    while (i < len) {
        while (i < len && line[i] == ' ') i++;
        if (i >= len) break;
        if (count == MAX_TOKENS) return -1;
        t[count].s = line + i;
        char *w = line + i;           /* compact in place: quotes are removed */
        int quoted = 0;
        for (; i < len && (quoted || line[i] != ' '); i++) {
            if (line[i] == '"') { quoted = !quoted; continue; }
            *w++ = line[i];
        }
        t[count].n = (size_t)(w - t[count].s);
        if (i < len) i++;             /* past the delimiter, so terminating the word cannot overwrite the next one */
        *w = 0;
        count++;
    }
    return count;
}

static void local(const xip_host *h, xip_cbor_w *w, const char *text)
{
    (void)h;
    xip_cbor_map(w, 1);
    xip_cbor_uint(w, 12);
    xip_cbor_text(w, text, xip_strlen(text));
}

static void help_text(out_t *o)
{
    puts_(o, "status | start NAME [TEXT] | stop NAME | events | reset | heap [regions|blocks|tasks|trace ...] | nv KEY[=VALUE] | wifi [ssid=NAME] [psk=SECRET] [start|stop]\n");
    puts_(o, "component.method key=value ... (values: 12, -3, true, hex:0a0b, text, \"two words\")\n");
    for (int i = 0; i < CAT_COMPS; i++) {
        puts_(o, cname(cat_comps[i].name)); puts_(o, ":");
        for (unsigned k = 0; k < cat_comps[i].count; k++) { put(o, " ", 1); puts_(o, cname(cat_methods[cat_comps[i].first + k].name)); }
        put(o, "\n", 1);
    }
}

/* Encode `value` for a field of kind `kind`; 0 on success. */
static int encode_value(xip_cbor_w *w, unsigned kind, const char *v, size_t n)
{
    uint64_t u;
    if (kind == CAT_K_MAP || kind == CAT_K_ARRAY) return -1;
    if (kind == CAT_K_TEXT) { xip_cbor_text(w, v, n); return 0; }
    if (kind == CAT_K_BOOL || kind == CAT_K_ANY) {
        if (n == 4 && same(v, "true", 4)) { xip_cbor_bool(w, 1); return 0; }
        if (n == 5 && same(v, "false", 5)) { xip_cbor_bool(w, 0); return 0; }
        if (kind == CAT_K_BOOL) return -1;
    }
    if (kind == CAT_K_BYTES || (kind == CAT_K_ANY && n > 4 && same(v, "hex:", 4))) {
        if (n >= 4 && same(v, "hex:", 4)) { v += 4; n -= 4; }
        if (n % 2) return -1;
        xip_cbor_head(w, 2, n / 2);
        for (size_t i = 0; i < n; i += 2) {
            int hi = hexval(v[i]), lo = hexval(v[i + 1]);
            if (hi < 0 || lo < 0) return -1;
            uint8_t b = (uint8_t)(hi << 4 | lo);
            xip_cbor_put(w, &b, 1);
        }
        return 0;
    }
    if (digits(v, n, &u)) { xip_cbor_uint(w, u); return 0; }
    if (n > 1 && v[0] == '-' && digits(v + 1, n - 1, &u) && u <= 0x7fffffffffffffffull) { xip_cbor_int(w, -(int64_t)u); return 0; }
    if (kind == CAT_K_ANY) { xip_cbor_text(w, v, n); return 0; }
    return -1;   /* a number was expected */
}

static void do_parse(const xip_host *h, mem_t *mem, const uint8_t *line_in, size_t line_len, xip_cbor_w *w)
{
    char *line = take(h, mem, line_len + 1);
    tok_t *t = take(h, mem, sizeof(tok_t) * MAX_TOKENS);
    if (!line || !t) { local(h, w, "error: out of memory"); return; }
    xip_copy(line, line_in, line_len);
    int count = tokenize(line, line_len, t);
    if (count < 0) { local(h, w, "error: too many words"); return; }
    if (count == 0) { local(h, w, ""); return; }

    /* component and method */
    const cat_comp *c = 0;
    const cat_method *m = 0;
    int used = 1;
    size_t dot = 0;
    while (dot < t[0].n && t[0].s[dot] != '.' && t[0].s[dot] != '=') dot++;
    uint64_t id;
    if (dot < t[0].n && t[0].s[dot] == '.') {
        c = find_comp(t[0].s, dot, digits(t[0].s, dot, &id) ? (unsigned long)id : 0, digits(t[0].s, dot, &id));
        if (c) m = find_method(c, t[0].s + dot + 1, t[0].n - dot - 1, digits(t[0].s + dot + 1, t[0].n - dot - 1, &id) ? (unsigned long)id : 0,
                               digits(t[0].s + dot + 1, t[0].n - dot - 1, &id));
    } else if (count > 1 && (c = find_comp(t[0].s, t[0].n, 0, 0)) != 0) {
        m = find_method(c, t[1].s, t[1].n, 0, 0);
        used = 2;
    } else {
        for (int i = 0; i < CAT_COMPS && !m; i++) {   /* a bare method: the first component that has it, main's first */
            c = &cat_comps[i];
            m = find_method(c, t[0].s, t[0].n, 0, 0);
        }
    }
    if (!c || !m) { local(h, w, "error: unknown method (try help)"); return; }

    /* fields: key=value words, and plain words filling the method's remaining declared fields in order */
    struct { unsigned tag, kind; const char *v; size_t n; } as[MAX_TOKENS];
    int na = 0, positional = 0;
    const cat_field *req = &cat_fields[m->req];
    for (int i = used; i < count; i++) {
        size_t eq = 0;
        while (eq < t[i].n && t[i].s[eq] != '=') eq++;
        if (eq == t[i].n) { positional++; continue; }
        if (eq == 0) { local(h, w, "error: expected key=value"); return; }
        const char *key = t[i].s;
        size_t kn = eq;
        uint64_t tag;
        const cat_field *f = 0;
        if (key[0] == '@' && digits(key + 1, kn - 1, &tag)) f = field_by_tag(req, m->nreq, (unsigned)tag);
        else if (digits(key, kn, &tag)) f = field_by_tag(req, m->nreq, (unsigned)tag);
        else {
            f = find_field(req, m->nreq, key, kn);
            if (!f) {
                /* a method whose last field is text (`start lora probe spi=2`): the word belongs to that text */
                if (m->nreq && req[m->nreq - 1].kind == CAT_K_TEXT) { positional++; continue; }
                local(h, w, "error: unknown field");
                return;
            }
            tag = f->tag;
        }
        t[i].s[eq] = 0;
        as[na].tag = (unsigned)tag; as[na].kind = f ? f->kind : CAT_K_ANY; as[na].v = key + eq + 1; as[na].n = t[i].n - eq - 1;
        na++;
    }
    if (positional) {
        unsigned fi = 0;
        for (int i = used; i < count; i++) {
            if (xip_strlen(t[i].s) != t[i].n) continue;     /* key=value: its key was cut at the '=' */
            while (fi < m->nreq) {                         /* the next declared field not given by name */
                int taken = 0;
                for (int k = 0; k < na; k++) taken |= as[k].tag == req[fi].tag;
                if (!taken) break;
                fi++;
            }
            if (fi >= m->nreq) { local(h, w, "error: too many words"); return; }
            const char *v = t[i].s;
            size_t vn = t[i].n;
            int last = fi + 1 == m->nreq;
            if (req[fi].kind == CAT_K_TEXT && last) {      /* the last text field takes the rest of the line */
                char *joined = take(h, mem, line_len + 1);
                if (!joined) { local(h, w, "error: out of memory"); return; }
                size_t jn = 0;
                for (int j = i; j < count; j++) {
                    if (j > i) joined[jn++] = ' ';
                    xip_copy(joined + jn, t[j].s, t[j].n);
                    jn += t[j].n;
                }
                v = joined; vn = jn; i = count;
            }
            as[na].tag = req[fi].tag; as[na].kind = req[fi].kind; as[na].v = v; as[na].n = vn;
            na++; fi++;
        }
    }
    uint8_t *out = take(h, mem, OUT_MAX);
    if (!out) { local(h, w, "error: out of memory"); return; }
    xip_cbor_w body;
    xip_cbor_init(&body, out, OUT_MAX);
    xip_cbor_map(&body, (size_t)na);
    for (int k = 0; k < na; k++) {
        xip_cbor_uint(&body, as[k].tag);
        if (encode_value(&body, as[k].kind, as[k].v, as[k].n)) { local(h, w, "error: bad value"); return; }
    }
    int blen = xip_cbor_done(&body);
    if (blen < 0) { local(h, w, "error: request too long"); return; }
    xip_cbor_map(w, 3);
    xip_cbor_uint(w, 1); xip_cbor_uint(w, c->id);
    xip_cbor_uint(w, 2); xip_cbor_uint(w, m->id);
    xip_cbor_uint(w, 5); xip_cbor_put(w, out, (size_t)blen);
}

/* ---- render ---- */
static void render_item(out_t *o, xip_cbor_r *r, const cat_field *f, int depth);

static void render_scalar(out_t *o, const xip_cbor_item *it, int quote)
{
    if (it->type == XIP_CBOR_UINT) putu(o, it->arg);
    else if (it->type == XIP_CBOR_NEGINT) { put(o, "-", 1); putu(o, it->arg + 1); }
    else if (it->type == XIP_CBOR_BOOL) puts_(o, it->arg ? "true" : "false");
    else if (it->type == XIP_CBOR_NULL) puts_(o, "null");
    else if (it->type == XIP_CBOR_TEXT) {
        int space = 0;
        for (size_t i = 0; i < it->n; i++) if (it->data[i] == ' ' || it->data[i] == '=') space = 1;
        if (quote && (space || !it->n)) put(o, "\"", 1);
        put(o, (const char *)it->data, it->n);
        if (quote && (space || !it->n)) put(o, "\"", 1);
    } else if (it->type == XIP_CBOR_BYTES) {
        puts_(o, "hex:");
        for (size_t i = 0; i < it->n; i++) { char b[2] = {"0123456789abcdef"[it->data[i] >> 4], "0123456789abcdef"[it->data[i] & 15]}; put(o, b, 2); }
    }
}

/* A map as `name=value` words. `rec` is the record the keys belong to (fields of cat_fields) or none. */
static void render_pairs(out_t *o, xip_cbor_r *r, size_t pairs, const cat_field *list, unsigned count, int depth)
{
    for (size_t i = 0; i < pairs; i++) {
        xip_cbor_item k;
        if (xip_cbor_next(r, &k)) return;
        const cat_field *f = k.type == XIP_CBOR_UINT ? field_by_tag(list, count, (unsigned)k.arg) : 0;
        if (i) put(o, " ", 1);
        if (f && f->name && same("text", cname(f->name), 4) && f->kind == CAT_K_TEXT) {
            xip_cbor_item v;
            if (xip_cbor_next(r, &v)) return;
            render_scalar(o, &v, 0);
            continue;
        }
        if (f && f->name) puts_(o, cname(f->name)); else { put(o, "@", 1); putn(o, (int64_t)k.arg); }
        put(o, "=", 1);
        render_item(o, r, f, depth + 1);
    }
}

static void render_item(out_t *o, xip_cbor_r *r, const cat_field *f, int depth)
{
    xip_cbor_item it;
    if (xip_cbor_next(r, &it)) return;
    const cat_record *rec = f && f->record ? &cat_records[f->record - 1] : 0;
    if (it.type == XIP_CBOR_MAP) {
        put(o, "{", 1);
        if (depth < 4) render_pairs(o, r, it.n, rec ? &cat_fields[rec->first] : cat_fields, rec ? rec->count : 0, depth);
        put(o, "}", 1);
    } else if (it.type == XIP_CBOR_ARRAY) {
        put(o, "[", 1);
        for (size_t i = 0; i < it.n && depth < 4; i++) { if (i) put(o, " ", 1); render_item(o, r, f && f->kind == CAT_K_ARRAY && f->record ? f : 0, depth + 1); }
        put(o, "]", 1);
    } else {
        render_scalar(o, &it, 1);
    }
}

static void do_render(const xip_host *h, mem_t *mem, const uint8_t *data, size_t len, xip_cbor_w *w)
{
    char *text = take(h, mem, OUT_MAX);
    if (!text) return;
    out_t o = {text, OUT_MAX, 0};
    int64_t comp = 0, meth = 0;
    xip_cbor_get_int(data, len, 1, &comp);
    xip_cbor_get_int(data, len, 2, &meth);
    const uint8_t *err; size_t en;
    xip_cbor_r res;
    if (xip_cbor_get_str(data, len, 7, XIP_CBOR_TEXT, &err, &en) == 0) {
        puts_(&o, "error: "); put(&o, (const char *)err, en);
    } else if (xip_cbor_get(data, len, 6, &res) == 0) {
        const cat_method *m = method_of((unsigned)comp, (unsigned)meth);
        const cat_field *resp = m ? &cat_fields[m->resp] : cat_fields;
        unsigned nresp = m ? m->nresp : 0;
        xip_cbor_item map;
        xip_cbor_r r = res;
        if (xip_cbor_next(&r, &map) || map.type != XIP_CBOR_MAP) puts_(&o, "error: bad answer");
        else if (map.n == 0) puts_(&o, "ok");
        else {
            size_t scalars = 0;
            for (size_t i = 0; i < map.n; i++) {
                xip_cbor_item k, v;
                if (xip_cbor_next(&r, &k)) break;
                const cat_field *f = k.type == XIP_CBOR_UINT ? field_by_tag(resp, nresp, (unsigned)k.arg) : 0;
                xip_cbor_r probe = r;
                if (xip_cbor_next(&probe, &v)) break;
                if (v.type == XIP_CBOR_ARRAY && f && f->record) {
                    /* a list of records: one line each, after the scalar words */
                    if (scalars) { put(&o, "\n", 1); scalars = 0; }
                    r = probe;
                    const cat_record *rec = &cat_records[f->record - 1];
                    for (size_t e = 0; e < v.n; e++) {
                        xip_cbor_item rm;
                        if (xip_cbor_next(&r, &rm) || rm.type != XIP_CBOR_MAP) break;
                        render_pairs(&o, &r, rm.n, &cat_fields[rec->first], rec->count, 1);
                        put(&o, "\n", 1);
                    }
                    continue;
                }
                if (scalars++) put(&o, " ", 1);
                if (f && f->name && f->kind == CAT_K_TEXT && same("text", cname(f->name), 4)) {
                    xip_cbor_next(&r, &v);
                    render_scalar(&o, &v, 0);
                    continue;
                }
                if (f && f->name) puts_(&o, cname(f->name)); else { put(&o, "@", 1); putn(&o, (int64_t)k.arg); }
                put(&o, "=", 1);
                render_item(&o, &r, f, 1);
            }
            if (scalars) put(&o, "\n", 1);
        }
        if (o.len && o.p[o.len - 1] != '\n') put(&o, "\n", 1);
    } else {
        puts_(&o, "ok\n");
    }
    if (!o.len) puts_(&o, "ok");
    if (o.p[o.len - 1] != '\n') put(&o, "\n", 1);
    xip_cbor_map(w, 1);
    xip_cbor_uint(w, 1);
    xip_cbor_text(w, o.p, o.len);
}

/* ---- the command line ---- */
#define M_LINE 3                      /* a line typed on the serial port (a packet) */

/* The bytes of the item stored under `key` of a CBOR map. */
static int raw_value(const uint8_t *map, size_t len, uint64_t key, const uint8_t **p, size_t *n)
{
    xip_cbor_r v;
    if (xip_cbor_get(map, len, key, &v)) return -1;
    xip_cbor_r after = v;
    if (xip_cbor_skip(&after)) return -1;
    *p = v.p;
    *n = v.len - after.len;
    return 0;
}

static const cat_record *record_named(const char *name)
{
    for (int i = 0; i < CAT_RECORDS; i++) if (same(name, cname(cat_records[i].name), xip_strlen(name))) return &cat_records[i];
    return 0;
}

/* A CBOR array of maps (the host's slot and event records): one line of `name=value` words each. */
static void render_list(out_t *o, const uint8_t *data, size_t len, const char *record)
{
    const cat_record *rec = record_named(record);
    xip_cbor_r r;
    xip_cbor_item it;
    xip_cbor_reader(&r, data, len);
    if (!rec || xip_cbor_next(&r, &it) || it.type != XIP_CBOR_ARRAY) { puts_(o, "error: bad answer\n"); return; }
    for (size_t i = 0; i < it.n; i++) {
        xip_cbor_item m;
        if (xip_cbor_next(&r, &m) || m.type != XIP_CBOR_MAP) break;
        render_pairs(o, &r, m.n, &cat_fields[rec->first], rec->count, 1);
        put(o, "\n", 1);
    }
}

static int word_is(const tok_t *t, const char *w) { return t->n == xip_strlen(w) && same(t->s, w, t->n); }

static void error_rc(out_t *o, const char *what, int rc)
{
    puts_(o, "error: "); puts_(o, what); puts_(o, " failed: "); putn(o, rc); put(o, "\n", 1);
}

/* `heap`: free memory, from heap_info (bytes) */
static void cmd_heap(const xip_host *h, const tok_t *t, int count, out_t *o)
{
    if (count > 1) {            /* `heap regions|blocks|tasks|trace start|trace dump|trace stop`: printed to the device log */
        int kind = word_is(&t[1], "regions") ? 0 : word_is(&t[1], "blocks") ? 1 : word_is(&t[1], "tasks") ? 2 :
                   (word_is(&t[1], "trace") && count > 2) ? (word_is(&t[2], "start") ? 3 : word_is(&t[2], "dump") ? 4 : word_is(&t[2], "stop") ? 5 : -1) : -1;
        if (kind < 0) { puts_(o, "error: heap [regions|blocks|tasks|trace start|dump|stop]\n"); return; }
        int rc = h->heap_dump(h->user, kind);
        if (rc) error_rc(o, "heap", rc); else puts_(o, "ok (in the device log)\n");
        return;
    }
    uint8_t info[64];
    int n = h->heap_info(h->user, info, sizeof(info));
    if (n < 0) { error_rc(o, "heap", n); return; }
    for (int k = 0; k < 6; k++) {                 /* (no table of pointers: a module image has no relocations) */
        int64_t v = 0;
        xip_cbor_get_int(info, (size_t)n, (uint64_t)k + 1, &v);
        puts_(o, k == 0 ? "free=" : k == 1 ? " largest=" : k == 2 ? " min_free=" : k == 3 ? " total=" : k == 4 ? " internal_free=" : " internal_largest=");
        putn(o, v);
    }
    puts_(o, "\n");
}

/* `nv KEY` shows a device setting, `nv KEY=VALUE` sets it, `nv KEY=` restores the default (nv: the application answers for its names) */
static void cmd_nv(const xip_host *h, const tok_t *t, int count, out_t *o)
{
    if (count != 2) { puts_(o, "error: nv KEY | nv KEY=VALUE | nv KEY=\n"); return; }
    size_t eq = 0;
    while (eq < t[1].n && t[1].s[eq] != '=') eq++;
    const int write = eq < t[1].n;
    uint8_t val[48];
    int rc = h->nv(h->user, (const uint8_t *)t[1].s, eq, write ? (const uint8_t *)t[1].s + eq + 1 : 0, write ? t[1].n - eq - 1 : 0, val, sizeof(val));
    if (rc == XIP_ERR_NOTFOUND && !write) { put(o, t[1].s, eq); puts_(o, " is unknown or not set\n"); return; }
    if (rc < 0) { error_rc(o, "nv", rc); return; }
    put(o, t[1].s, eq);
    if (write) { puts_(o, write && t[1].n == eq + 1 ? " restored to the default (applies at the next start)\n" : " set (applies at the next start)\n"); return; }
    puts_(o, "="); put(o, (const char *)val, (size_t)rc); puts_(o, "\n");
}

/* `wifi scan`: one line per network in range, strongest first (wifi_scan: the first call starts it, a later one has the list) */
static void wifi_scan_list(const xip_host *h, out_t *o)
{
    uint8_t buf[1024];
    int n = h->wifi_scan(h->user, buf, sizeof(buf));
    if (n == XIP_ERR_BUSY) { puts_(o, "scanning; run `wifi scan` again in a few seconds\n"); return; }
    if (n < 0) { error_rc(o, "wifi scan", n); return; }
    xip_cbor_r r;
    xip_cbor_item arr;
    xip_cbor_reader(&r, buf, (size_t)n);
    if (xip_cbor_next(&r, &arr) || arr.type != XIP_CBOR_ARRAY) { puts_(o, "error: bad scan answer\n"); return; }
    for (size_t i = 0; i < arr.n; i++) {
        const uint8_t *entry = r.p;
        xip_cbor_r skip = r;
        if (xip_cbor_skip(&skip)) break;
        const size_t elen = (size_t)(skip.p - entry);
        const uint8_t *name, *bssid;
        size_t nn, bn;
        int64_t rssi = 0, ch = 0, auth = 0;
        xip_cbor_get_str(entry, elen, 1, XIP_CBOR_BYTES, &name, &nn);
        xip_cbor_get_int(entry, elen, 3, &rssi);
        xip_cbor_get_int(entry, elen, 4, &ch);
        xip_cbor_get_int(entry, elen, 5, &auth);
        puts_(o, "ssid=\""); put(o, (const char *)name, nn); puts_(o, "\" ch="); putn(o, ch);
        puts_(o, " rssi="); putn(o, rssi); puts_(o, " auth="); putn(o, auth);
        if (xip_cbor_get_str(entry, elen, 2, XIP_CBOR_BYTES, &bssid, &bn) == 0 && bn == 6) {
            char hex[17 + 7] = " bssid=";
            for (int k = 0; k < 6; k++) {
                hex[7 + k * 3] = "0123456789abcdef"[bssid[k] >> 4];
                hex[8 + k * 3] = "0123456789abcdef"[bssid[k] & 15];
                if (k < 5) hex[9 + k * 3] = ':';
            }
            put(o, hex, 7 + 17);
        }
        put(o, "\n", 1);
        r = skip;
    }
}

/* `wifi ssid=NAME psk=SECRET` stores the device's network (wifi_config: esp-wifi-loader's settings, never an image; both in
 * one line, `psk=` alone keeps the name, an empty `ssid=` clears it); `wifi start` joins it if a scan sees it, else the flash
 * network; `wifi` shows the state; `wifi stop` leaves. The network is optional: installs work without it. */
static void cmd_wifi(const xip_host *h, const tok_t *t, int count, out_t *o)
{
    if (!(h->features & XIP_FEAT_WIFI)) { puts_(o, "error: this host has no Wi-Fi\n"); return; }
    uint8_t info[128];
    int n = h->wifi_info(h->user, info, sizeof(info));
    const uint8_t *text; size_t tn;
    char ssid[40], psk[72];
    size_t sn = 0, pn = 0;
    int set_ssid = 0, set_psk = 0;
    if (n > 0 && xip_cbor_get_str(info, (size_t)n, 5, XIP_CBOR_TEXT, &text, &tn) == 0 && tn < sizeof(ssid)) { xip_copy(ssid, text, tn); sn = tn; }
    for (int i = 1; i < count; i++) {
        int rc = 0;
        if (word_is(&t[i], "scan")) {
            wifi_scan_list(h, o);
            return;
        } else if (word_is(&t[i], "stop")) {
            rc = h->wifi_stop(h->user);
        } else if (word_is(&t[i], "start")) {
            rc = h->wifi_start(h->user, (const uint8_t *)"", 0, (const uint8_t *)"", 0);
        } else {
            size_t eq = 0;
            while (eq < t[i].n && t[i].s[eq] != '=') eq++;
            const size_t vn = eq < t[i].n ? t[i].n - eq - 1 : 0;
            if (eq == 4 && same(t[i].s, "ssid", 4) && vn < sizeof(ssid)) { xip_copy(ssid, t[i].s + eq + 1, vn); sn = vn; set_ssid = 1; }
            else if (eq == 3 && same(t[i].s, "psk", 3) && vn < sizeof(psk)) { xip_copy(psk, t[i].s + eq + 1, vn); pn = vn; set_psk = 1; }
            else { puts_(o, "error: wifi [ssid=NAME] [psk=SECRET] [start|stop] (empty value clears)\n"); return; }
        }
        if (rc) { error_rc(o, "wifi", rc); return; }
    }
    if (set_ssid || set_psk) {
        int rc = h->wifi_config(h->user, (const uint8_t *)ssid, sn, (const uint8_t *)psk, set_ssid && !set_psk ? 0 : pn);
        if (rc) { error_rc(o, "wifi", rc); return; }
    }
    n = h->wifi_info(h->user, info, sizeof(info));
    int64_t state = 0, rssi = 0;
    if (n > 0) xip_cbor_get_int(info, (size_t)n, 1, &state);
    puts_(o, "state="); puts_(o, state == 2 ? "up" : state == 1 ? "joining" : "off");
    if (state == 2 && xip_cbor_get_str(info, (size_t)n, 2, XIP_CBOR_TEXT, &text, &tn) == 0) { puts_(o, " ssid=\""); put(o, (const char *)text, tn); puts_(o, "\""); }
    if (state == 2 && xip_cbor_get_str(info, (size_t)n, 3, XIP_CBOR_TEXT, &text, &tn) == 0) { puts_(o, " ip="); put(o, (const char *)text, tn); }
    if (state == 2 && xip_cbor_get_int(info, (size_t)n, 4, &rssi) == 0) { puts_(o, " rssi="); putn(o, rssi); }
    puts_(o, " saved_ssid=\"");
    if (n > 0 && xip_cbor_get_str(info, (size_t)n, 5, XIP_CBOR_TEXT, &text, &tn) == 0) put(o, (const char *)text, tn); else puts_(o, "(flash network)");
    xip_cbor_r pv; xip_cbor_item pit;
    const int psk_set = n > 0 && xip_cbor_get(info, (size_t)n, 6, &pv) == 0 && xip_cbor_next(&pv, &pit) == 0 && pit.type == XIP_CBOR_BOOL && pit.arg;
    puts_(o, psk_set ? "\" psk=set\n" : "\" psk=unset\n");
}

/* The console waits for nothing. A line is a packet, the request to a module is a packet with an id, and what comes back is
 * whatever packets the module sends, whenever it sends them: one answer, several lines, a stream, a message with the same id
 * as an earlier request, an event nobody asked for. Each is printed as it arrives, like a background app writing to a terminal
 * or the replies of a multiplexed connection. The only memory is the next id. */
typedef struct {
    uint32_t next_id;
} cstate_t;

static void render_answer(const xip_host *h, mem_t *mem, int64_t comp, int64_t meth, const uint8_t *res, size_t rn, const char *err, out_t *o)
{
    uint8_t *tmp = take(h, mem, RESP_MAX);
    uint8_t *out = take(h, mem, RESP_MAX);
    if (!tmp || !out) { puts_(o, "error: out of memory\n"); return; }
    /* the answer for render: {1: component, 2: method, 6: result} or {7: error} */
    xip_cbor_w w;
    xip_cbor_init(&w, tmp, RESP_MAX);
    xip_cbor_map(&w, 3);
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, (uint64_t)comp);
    xip_cbor_uint(&w, 2); xip_cbor_uint(&w, (uint64_t)meth);
    if (err) { xip_cbor_uint(&w, 7); xip_cbor_text(&w, err, xip_strlen(err)); }
    else { xip_cbor_uint(&w, 6); if (rn) xip_cbor_put(&w, res, rn); else xip_cbor_map(&w, 0); }
    int tl = xip_cbor_done(&w);
    if (tl <= 0) { puts_(o, "error: answer too large\n"); return; }
    xip_cbor_w r;
    xip_cbor_init(&r, out, RESP_MAX);
    do_render(h, mem, tmp, (size_t)tl, &r);
    int rl = xip_cbor_done(&r);
    const uint8_t *text; size_t tn;
    if (rl > 0 && xip_cbor_get_str(out, (size_t)rl, 1, XIP_CBOR_TEXT, &text, &tn) == 0) put(o, (const char *)text, tn);
}

/* The manifest of the installed module (abi/API.md, module_manifest) is the module's own word on which methods it has; when
 * it has one, a method it does not list is refused here instead of being sent. No manifest, no such slot or a manifest too
 * large for the buffer: the catalog's word stands. */
static int manifest_allows(const xip_host *h, mem_t *mem, int64_t component, int64_t method, out_t *o)
{
    const char *name = 0;
    for (int i = 0; i < CAT_COMPS; i++) if (cat_comps[i].id == component) name = cname(cat_comps[i].name);
    if (!name || !(h->features & XIP_FEAT_MODULES) || !XIP_HOST_HAS(h, module_manifest)) return 1;
    uint8_t *m = take(h, mem, 1024);
    if (!m) return 1;
    int n = h->module_manifest(h->user, (const uint8_t *)name, xip_strlen(name), m, 1024);
    xip_cbor_r r;
    xip_cbor_item it;
    if (n <= 0 || xip_cbor_get(m, (size_t)n, 3, &r) || xip_cbor_next(&r, &it) || it.type != XIP_CBOR_ARRAY) return 1;
    for (size_t i = 0; i < it.n; i++) {
        xip_cbor_r start = r;                                       /* {1: id, 2: name, 3: request fields, 4: response fields} */
        int64_t id = -1;
        if (xip_cbor_skip(&r)) return 1;
        if (xip_cbor_get_int(start.p, start.len - r.len, 1, &id) == 0 && id == method) return 1;
    }
    puts_(o, "error: "); puts_(o, name); puts_(o, " does not list method "); putn(o, method); puts_(o, " in its manifest\n");
    return 0;
}

/* `NAME.METHOD key=value`: the request payload by the catalog, sent as a packet to the module. 1 when it was sent (the answer
 * comes later); 0 when `o` already has the text (a syntax error, or nobody to send to). */
static int cmd_call(const xip_host *h, mem_t *mem, cstate_t *st, const uint8_t *line, size_t n, out_t *o)
{
    uint8_t *req = take(h, mem, RESP_MAX);
    if (!req) { puts_(o, "error: out of memory\n"); return 0; }
    xip_cbor_w w;
    xip_cbor_init(&w, req, RESP_MAX);
    do_parse(h, mem, line, n, &w);
    int len = xip_cbor_done(&w);
    const uint8_t *text; size_t tn;
    if (len <= 0) { puts_(o, "error: bad request\n"); return 0; }
    if (xip_cbor_get_str(req, (size_t)len, 12, XIP_CBOR_TEXT, &text, &tn) == 0) {     /* answered by the parser */
        put(o, (const char *)text, tn);
        return 0;
    }
    int64_t comp = 0, meth = 0;
    const uint8_t *fields; size_t fn;
    if (xip_cbor_get_int(req, (size_t)len, 1, &comp) || xip_cbor_get_int(req, (size_t)len, 2, &meth) ||
        raw_value(req, (size_t)len, 5, &fields, &fn)) { puts_(o, "error: bad request\n"); return 0; }

    if (!manifest_allows(h, mem, comp, meth, o)) return 0;
    uint32_t pkt = 0;
    uint8_t *window = 0;
    size_t room = 0;
    if (h->pkt_alloc(h->user, &pkt) != 0) { puts_(o, "error: no packet\n"); return 0; }
    if (h->pkt_buf(h->user, pkt, &window, &room) != 0 || fn > room) {
        h->pkt_free(h->user, pkt);
        puts_(o, "error: request too long\n");
        return 0;
    }
    xip_copy(window, fields, fn);
    if (++st->next_id == 0) st->next_id = 1;
    int rc = h->send(h->user, pkt, fn, (uint32_t)comp, (int)comp, (int)meth, st->next_id);
    if (rc == XIP_ERR_NOTFOUND) { puts_(o, "error: module not running (start it)\n"); return 0; }
    if (rc == XIP_ERR_BUSY) { puts_(o, "error: busy (the module has no room for it yet)\n"); return 0; }
    if (rc) { error_rc(o, "send", rc); return 0; }
    return 1;
}

/* One line in; the text goes to the out_t. Returns 1 when the answer is still to come. */
static int do_line(const xip_host *h, mem_t *mem, cstate_t *st, const uint8_t *line_in, size_t n, out_t *o)
{
    char *line = take(h, mem, n + 1);
    tok_t *t = take(h, mem, sizeof(tok_t) * MAX_TOKENS);
    if (!line || !t) { puts_(o, "error: out of memory\n"); return 0; }
    xip_copy(line, line_in, n);
    int count = tokenize(line, n, t);
    if (count < 0) { puts_(o, "error: too many words\n"); return 0; }
    if (count == 0) return 0;
    uint8_t *buf = take(h, mem, RESP_MAX);
    if (!buf) { puts_(o, "error: out of memory\n"); return 0; }
    int len;
    if (word_is(&t[0], "help")) {
        help_text(o);
    } else if (word_is(&t[0], "nv")) {
        if (XIP_HOST_HAS(h, nv)) cmd_nv(h, t, count, o); else puts_(o, "error: this host has no device settings\n");
    } else if (word_is(&t[0], "heap")) {
        cmd_heap(h, t, count, o);
    } else if (word_is(&t[0], "wifi")) {
        cmd_wifi(h, t, count, o);
    } else if (word_is(&t[0], "status")) {
        cmd_status(h, o);
        return o->len == 0;
    } else if (word_is(&t[0], "events") || word_is(&t[0], "reset") ||
               word_is(&t[0], "start") || word_is(&t[0], "stop")) {
        if (!(h->features & XIP_FEAT_MODULES)) { puts_(o, "error: this host has no module control\n"); return 0; }
        if (word_is(&t[0], "events")) {
            len = h->events_info(h->user, buf, RESP_MAX);
            if (len > 0) render_list(o, buf, (size_t)len, "event");
        } else if (word_is(&t[0], "reset")) {
            len = h->reset(h->user);
            if (len) error_rc(o, "reset", len); else puts_(o, "ok\n");
        } else if (count < 2) {
            puts_(o, "error: name is required\n");
        } else {
            int rc;
            if (word_is(&t[0], "stop")) {
                rc = h->module_stop(h->user, (const uint8_t *)t[1].s, t[1].n);
            } else {                       /* the rest of the line is the start request */
                char *rest = take(h, mem, n + 1);
                if (!rest) { puts_(o, "error: out of memory\n"); return 0; }
                size_t rn = 0;
                for (int j = 2; j < count; j++) {
                    if (j > 2) rest[rn++] = ' ';
                    xip_copy(rest + rn, t[j].s, t[j].n);
                    rn += t[j].n;
                }
                rc = h->module_start(h->user, (const uint8_t *)t[1].s, t[1].n, (const uint8_t *)rest, rn);
            }
            if (rc == XIP_ERR_NOTFOUND) puts_(o, "error: unknown module\n");
            else if (rc) error_rc(o, word_is(&t[0], "stop") ? "stop" : "start", rc);
            else puts_(o, "ok\n");
        }
    } else {
        if (cmd_call(h, mem, st, line_in, n, o)) return 1;
    }
    if (!o->len) puts_(o, "ok\n");
    else if (o->p[o->len - 1] != '\n') put(o, "\n", 1);
    return 0;
}

static void say(const xip_host *h, const out_t *o)
{
    if (o->len && (h->features & XIP_FEAT_SERIAL)) (void)h->serial_write(h->user, (const uint8_t *)o->p, o->len);
}

static int handle(const xip_host *h, mem_t *mem, void *state, const xip_call *c)
{
    cstate_t *st = state;
    if (c->component == XIP_HOST_COMPONENT) {
        if (c->method == XIP_HOST_START && (h->features & XIP_FEAT_SERIAL)) (void)h->serial_open(h->user, C_CONSOLE, M_LINE);
        else if (c->method == XIP_HOST_STOP && (h->features & XIP_FEAT_SERIAL)) (void)h->serial_close(h->user);
        return XIP_RET_CONTINUE;
    }
    /* a message from a module: print it as the answer to (component, method) it carries; it may be one of many */
    if (c->pkt && c->from != XIP_ADDR_MAIN && !(c->component == C_CONSOLE && c->method == M_LINE)) {
        char *text = take(h, mem, OUT_MAX);
        if (text) {
            out_t o = {text, OUT_MAX, 0};
            render_answer(h, mem, c->component, c->method, c->data, c->len, 0, &o);
            if (!o.len) puts_(&o, "ok");
            if (o.p[o.len - 1] != '\n') put(&o, "\n", 1);
            say(h, &o);
        }
        return XIP_RET_CONTINUE;
    }
    if (c->component != C_CONSOLE) return XIP_RET_CONTINUE;
    if (c->method == M_LINE && c->pkt) {
        char *text = take(h, mem, OUT_MAX);
        if (!text) {                    /* memory is refused: say so, the reply cannot be built */
            if (h->features & XIP_FEAT_SERIAL) (void)h->serial_write(h->user, (const uint8_t *)"error: out of memory\n", 21);
            return XIP_RET_CONTINUE;
        }
        out_t o = {text, OUT_MAX, 0};
        if (do_line(h, mem, st, c->data, c->len, &o)) return XIP_RET_CONTINUE;      /* what the module says comes as packets */
        say(h, &o);
        return XIP_RET_CONTINUE;
    }
    if (!(c->flags & 1)) return XIP_RET_CONTINUE;
    uint8_t *buf = take(h, mem, RESP_MAX);
    if (!buf) return XIP_RET_CONTINUE;
    xip_cbor_w w;
    xip_cbor_init(&w, buf, RESP_MAX);
    if (c->method == M_PARSE) {
        const uint8_t *line; size_t n;
        if (xip_cbor_get_str(c->data, c->len, 1, XIP_CBOR_TEXT, &line, &n) == 0) do_parse(h, mem, line, n, &w);
        else local(h, &w, "error: no line");
    } else if (c->method == M_RENDER) {
        do_render(h, mem, c->data, c->len, &w);
    } else {
        return XIP_RET_CONTINUE;
    }
    int n = xip_cbor_done(&w);
    if (n > 0) h->respond(h->user, buf, (size_t)n);
    return XIP_RET_CONTINUE;
}

XIP_HANDLER(console)(const xip_host *h, void *state, const xip_call *c)
{
    mem_t m = {{0}, 0};
    const int rc = handle(h, &m, state, c);
    give_back(h, &m);        /* what the call took (text buffers, tokens) goes back whether it ended early or not */
    return rc;
}
