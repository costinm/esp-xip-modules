/* SPDX-License-Identifier: Apache-2.0 */
/* SPDX-License-Identifier: Apache-2.0 */
/* mod_console: the text interface of a device (API.md in this directory), and the only place text is made or read.
 *
 * It owns the serial port through the host (serial_open): every line typed arrives as a request, and what the handler
 * answers is printed. A line is `status`, `start NAME [TEXT]`, `stop NAME`, `events`, `reset`, `wifi ...`, `loader ...`,
 * `help`, or `component.method key=value ...`: the last turns into the request payload of the module `component`
 * (tagged CBOR, named by catalog.h, generated from the modules' API.md files), calls it with the host's call(), and
 * turns its answer back into text. The host moves bytes and structured records; it formats nothing. */
#include "espmod_module.h"
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
        x = x * 10 + (uint64_t)(s[i] - '0');
    }
    *v = x;
    return 1;
}

static int hexval(char c) { return c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1; }

/* Output buffer for text. */
typedef struct { char *p; size_t cap, len; } out_t;
static void put(out_t *o, const char *s, size_t n) { for (size_t i = 0; i < n && o->len + 1 < o->cap; i++) o->p[o->len++] = s[i]; }
static void puts_(out_t *o, const char *s) { put(o, s, em_strlen(s)); }
static void putu(out_t *o, uint64_t v) { char d[24]; int k = 0; do { d[k++] = (char)('0' + v % 10); v /= 10; } while (v); while (k) put(o, &d[--k], 1); }
static void putn(out_t *o, int64_t v) { char b[24]; size_t n = em_itoa(b, (long)v); put(o, b, n); }

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

static void local(const espmod_host *h, em_cbor_w *w, const char *text)
{
    (void)h;
    em_cbor_map(w, 1);
    em_cbor_uint(w, 12);
    em_cbor_text(w, text, em_strlen(text));
}

static void help_text(out_t *o)
{
    puts_(o, "status | start NAME [TEXT] | stop NAME | events | reset | wifi [ssid=NAME] [psk=SECRET] [start|stop] | loader start NAME | loader stop\n");
    puts_(o, "component.method key=value ... (values: 12, -3, true, hex:0a0b, text, \"two words\")\n");
    for (int i = 0; i < CAT_COMPS; i++) {
        puts_(o, cname(cat_comps[i].name)); puts_(o, ":");
        for (unsigned k = 0; k < cat_comps[i].count; k++) { put(o, " ", 1); puts_(o, cname(cat_methods[cat_comps[i].first + k].name)); }
        put(o, "\n", 1);
    }
}

/* Encode `value` for a field of kind `kind`; 0 on success. */
static int encode_value(em_cbor_w *w, unsigned kind, const char *v, size_t n)
{
    uint64_t u;
    if (kind == CAT_K_MAP || kind == CAT_K_ARRAY) return -1;
    if (kind == CAT_K_TEXT) { em_cbor_text(w, v, n); return 0; }
    if (kind == CAT_K_BOOL || kind == CAT_K_ANY) {
        if (n == 4 && same(v, "true", 4)) { em_cbor_bool(w, 1); return 0; }
        if (n == 5 && same(v, "false", 5)) { em_cbor_bool(w, 0); return 0; }
        if (kind == CAT_K_BOOL) return -1;
    }
    if (kind == CAT_K_BYTES || (kind == CAT_K_ANY && n > 4 && same(v, "hex:", 4))) {
        if (n >= 4 && same(v, "hex:", 4)) { v += 4; n -= 4; }
        if (n % 2) return -1;
        em_cbor_head(w, 2, n / 2);
        for (size_t i = 0; i < n; i += 2) {
            int hi = hexval(v[i]), lo = hexval(v[i + 1]);
            if (hi < 0 || lo < 0) return -1;
            uint8_t b = (uint8_t)(hi << 4 | lo);
            em_cbor_put(w, &b, 1);
        }
        return 0;
    }
    if (digits(v, n, &u)) { em_cbor_uint(w, u); return 0; }
    if (n > 1 && v[0] == '-' && digits(v + 1, n - 1, &u) && u <= 0x7fffffffffffffffull) { em_cbor_int(w, -(int64_t)u); return 0; }
    if (kind == CAT_K_ANY) { em_cbor_text(w, v, n); return 0; }
    return -1;   /* a number was expected */
}

static void do_parse(const espmod_host *h, const uint8_t *line_in, size_t line_len, em_cbor_w *w)
{
    char *line = h->alloc(h->user, line_len + 1, 1);
    tok_t *t = h->alloc(h->user, sizeof(tok_t) * MAX_TOKENS, 8);
    char *msg = h->alloc(h->user, OUT_MAX, 1);
    if (!line || !t || !msg) { local(h, w, "error: out of memory"); return; }
    em_copy(line, line_in, line_len);
    int count = tokenize(line, line_len, t);
    if (count < 0) { local(h, w, "error: too many words"); return; }
    if (count == 0) { local(h, w, ""); return; }
    out_t o = {msg, OUT_MAX, 0};
    (void)o;

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
            if (em_strlen(t[i].s) != t[i].n) continue;     /* key=value: its key was cut at the '=' */
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
                char *joined = h->alloc(h->user, line_len + 1, 1);
                if (!joined) { local(h, w, "error: out of memory"); return; }
                size_t jn = 0;
                for (int j = i; j < count; j++) {
                    if (j > i) joined[jn++] = ' ';
                    em_copy(joined + jn, t[j].s, t[j].n);
                    jn += t[j].n;
                }
                v = joined; vn = jn; i = count;
            }
            as[na].tag = req[fi].tag; as[na].kind = req[fi].kind; as[na].v = v; as[na].n = vn;
            na++; fi++;
        }
    }
    uint8_t *out = h->alloc(h->user, OUT_MAX, 4);
    if (!out) { local(h, w, "error: out of memory"); return; }
    em_cbor_w body;
    em_cbor_init(&body, out, OUT_MAX);
    em_cbor_map(&body, (size_t)na);
    for (int k = 0; k < na; k++) {
        em_cbor_uint(&body, as[k].tag);
        if (encode_value(&body, as[k].kind, as[k].v, as[k].n)) { local(h, w, "error: bad value"); return; }
    }
    int blen = em_cbor_done(&body);
    if (blen < 0) { local(h, w, "error: request too long"); return; }
    em_cbor_map(w, 3);
    em_cbor_uint(w, 1); em_cbor_uint(w, c->id);
    em_cbor_uint(w, 2); em_cbor_uint(w, m->id);
    em_cbor_uint(w, 5); em_cbor_put(w, out, (size_t)blen);
}

/* ---- render ---- */
static void render_item(out_t *o, em_cbor_r *r, const cat_field *f, int depth);

static void render_scalar(out_t *o, const em_cbor_item *it, int quote)
{
    if (it->type == EM_CBOR_UINT) putu(o, it->arg);
    else if (it->type == EM_CBOR_NEGINT) { put(o, "-", 1); putu(o, it->arg + 1); }
    else if (it->type == EM_CBOR_BOOL) puts_(o, it->arg ? "true" : "false");
    else if (it->type == EM_CBOR_NULL) puts_(o, "null");
    else if (it->type == EM_CBOR_TEXT) {
        int space = 0;
        for (size_t i = 0; i < it->n; i++) if (it->data[i] == ' ' || it->data[i] == '=') space = 1;
        if (quote && (space || !it->n)) put(o, "\"", 1);
        put(o, (const char *)it->data, it->n);
        if (quote && (space || !it->n)) put(o, "\"", 1);
    } else if (it->type == EM_CBOR_BYTES) {
        puts_(o, "hex:");
        for (size_t i = 0; i < it->n; i++) { char b[2] = {"0123456789abcdef"[it->data[i] >> 4], "0123456789abcdef"[it->data[i] & 15]}; put(o, b, 2); }
    }
}

/* A map as `name=value` words. `rec` is the record the keys belong to (fields of cat_fields) or none. */
static void render_pairs(out_t *o, em_cbor_r *r, size_t pairs, const cat_field *list, unsigned count, int depth)
{
    for (size_t i = 0; i < pairs; i++) {
        em_cbor_item k;
        if (em_cbor_next(r, &k)) return;
        const cat_field *f = k.type == EM_CBOR_UINT ? field_by_tag(list, count, (unsigned)k.arg) : 0;
        if (i) put(o, " ", 1);
        if (f && f->name && same("text", cname(f->name), 4) && f->kind == CAT_K_TEXT) {
            em_cbor_item v;
            if (em_cbor_next(r, &v)) return;
            render_scalar(o, &v, 0);
            continue;
        }
        if (f && f->name) puts_(o, cname(f->name)); else { put(o, "@", 1); putn(o, (int64_t)k.arg); }
        put(o, "=", 1);
        render_item(o, r, f, depth + 1);
    }
}

static void render_item(out_t *o, em_cbor_r *r, const cat_field *f, int depth)
{
    em_cbor_item it;
    if (em_cbor_next(r, &it)) return;
    const cat_record *rec = f && f->record ? &cat_records[f->record - 1] : 0;
    if (it.type == EM_CBOR_MAP) {
        put(o, "{", 1);
        if (depth < 4) render_pairs(o, r, it.n, rec ? &cat_fields[rec->first] : cat_fields, rec ? rec->count : 0, depth);
        put(o, "}", 1);
    } else if (it.type == EM_CBOR_ARRAY) {
        put(o, "[", 1);
        for (size_t i = 0; i < it.n && depth < 4; i++) { if (i) put(o, " ", 1); render_item(o, r, f && f->kind == CAT_K_ARRAY && f->record ? f : 0, depth + 1); }
        put(o, "]", 1);
    } else {
        render_scalar(o, &it, 1);
    }
}

static void do_render(const espmod_host *h, const uint8_t *data, size_t len, em_cbor_w *w)
{
    char *text = h->alloc(h->user, OUT_MAX, 1);
    if (!text) return;
    out_t o = {text, OUT_MAX, 0};
    int64_t comp = 0, meth = 0;
    em_cbor_get_int(data, len, 1, &comp);
    em_cbor_get_int(data, len, 2, &meth);
    const uint8_t *err; size_t en;
    em_cbor_r res;
    if (em_cbor_get_str(data, len, 7, EM_CBOR_TEXT, &err, &en) == 0) {
        puts_(&o, "error: "); put(&o, (const char *)err, en);
    } else if (em_cbor_get(data, len, 6, &res) == 0) {
        const cat_method *m = method_of((unsigned)comp, (unsigned)meth);
        const cat_field *resp = m ? &cat_fields[m->resp] : cat_fields;
        unsigned nresp = m ? m->nresp : 0;
        em_cbor_item map;
        em_cbor_r r = res;
        if (em_cbor_next(&r, &map) || map.type != EM_CBOR_MAP) puts_(&o, "error: bad answer");
        else if (map.n == 0) puts_(&o, "ok");
        else {
            size_t scalars = 0;
            for (size_t i = 0; i < map.n; i++) {
                em_cbor_item k, v;
                if (em_cbor_next(&r, &k)) break;
                const cat_field *f = k.type == EM_CBOR_UINT ? field_by_tag(resp, nresp, (unsigned)k.arg) : 0;
                em_cbor_r probe = r;
                if (em_cbor_next(&probe, &v)) break;
                if (v.type == EM_CBOR_ARRAY && f && f->record) {
                    /* a list of records: one line each, after the scalar words */
                    if (scalars) { put(&o, "\n", 1); scalars = 0; }
                    r = probe;
                    const cat_record *rec = &cat_records[f->record - 1];
                    for (size_t e = 0; e < v.n; e++) {
                        em_cbor_item rm;
                        if (em_cbor_next(&r, &rm) || rm.type != EM_CBOR_MAP) break;
                        render_pairs(&o, &r, rm.n, &cat_fields[rec->first], rec->count, 1);
                        put(&o, "\n", 1);
                    }
                    continue;
                }
                if (scalars++) put(&o, " ", 1);
                if (f && f->name && f->kind == CAT_K_TEXT && same("text", cname(f->name), 4)) {
                    em_cbor_next(&r, &v);
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
    em_cbor_map(w, 1);
    em_cbor_uint(w, 1);
    em_cbor_text(w, o.p, o.len);
}

/* ---- the command line ---- */
#define M_LINE 3

/* The bytes of the item stored under `key` of a CBOR map. */
static int raw_value(const uint8_t *map, size_t len, uint64_t key, const uint8_t **p, size_t *n)
{
    em_cbor_r v;
    if (em_cbor_get(map, len, key, &v)) return -1;
    em_cbor_r after = v;
    if (em_cbor_skip(&after)) return -1;
    *p = v.p;
    *n = v.len - after.len;
    return 0;
}

static const cat_record *record_named(const char *name)
{
    for (int i = 0; i < CAT_RECORDS; i++) if (same(name, cname(cat_records[i].name), em_strlen(name))) return &cat_records[i];
    return 0;
}

/* A CBOR array of maps (the host's slot and event records): one line of `name=value` words each. */
static void render_list(out_t *o, const uint8_t *data, size_t len, const char *record)
{
    const cat_record *rec = record_named(record);
    em_cbor_r r;
    em_cbor_item it;
    em_cbor_reader(&r, data, len);
    if (!rec || em_cbor_next(&r, &it) || it.type != EM_CBOR_ARRAY) { puts_(o, "error: bad answer\n"); return; }
    for (size_t i = 0; i < it.n; i++) {
        em_cbor_item m;
        if (em_cbor_next(&r, &m) || m.type != EM_CBOR_MAP) break;
        render_pairs(o, &r, m.n, &cat_fields[rec->first], rec->count, 1);
        put(o, "\n", 1);
    }
}

static size_t setting(const espmod_host *h, const char *key, char *buf, size_t cap)
{
    size_t n = 0;
    if (h->get_setting(h->user, (const uint8_t *)key, em_strlen(key), (uint8_t *)buf, cap - 1, &n) != 0) n = 0;
    buf[n] = 0;
    return n;
}

static int word_is(const tok_t *t, const char *w) { return t->n == em_strlen(w) && same(t->s, w, t->n); }

static void error_rc(out_t *o, const char *what, int rc)
{
    puts_(o, "error: "); puts_(o, what); puts_(o, " failed: "); putn(o, rc); put(o, "\n", 1);
}

/* `wifi ssid=NAME psk=SECRET` keeps the network in this module's settings (never in an image); `wifi start` joins it
 * (empty ssid: the flash network), `wifi` shows the state, `wifi stop` leaves. */
static void cmd_wifi(const espmod_host *h, const tok_t *t, int count, out_t *o)
{
    if (!(h->features & ESPMOD_FEAT_WIFI)) { puts_(o, "error: this host has no Wi-Fi\n"); return; }
    for (int i = 1; i < count; i++) {
        int rc;
        if (word_is(&t[i], "stop")) {
            rc = h->wifi_stop(h->user);
        } else if (word_is(&t[i], "start")) {
            char ssid[40], psk[72];
            size_t sn = setting(h, "ssid", ssid, sizeof(ssid)), pn = setting(h, "psk", psk, sizeof(psk));
            rc = h->wifi_start(h->user, (const uint8_t *)ssid, sn, (const uint8_t *)psk, pn);
        } else {
            size_t eq = 0;
            while (eq < t[i].n && t[i].s[eq] != '=') eq++;
            const char *key = eq == 4 && same(t[i].s, "ssid", 4) ? "ssid" : eq == 3 && same(t[i].s, "psk", 3) ? "psk" : 0;
            if (!key || eq == t[i].n || !(h->features & ESPMOD_FEAT_SETTINGS)) {
                puts_(o, "error: wifi [ssid=NAME] [psk=SECRET] [start|stop] (empty value clears)\n");
                return;
            }
            rc = h->set_setting(h->user, (const uint8_t *)key, em_strlen(key), (const uint8_t *)t[i].s + eq + 1, t[i].n - eq - 1);
        }
        if (rc) { error_rc(o, "wifi", rc); return; }
    }
    uint8_t info[96];
    int n = h->wifi_info(h->user, info, sizeof(info));
    int64_t state = 0, rssi = 0;
    const uint8_t *text; size_t tn;
    if (n > 0) em_cbor_get_int(info, (size_t)n, 1, &state);
    puts_(o, "state="); puts_(o, state == 2 ? "up" : state == 1 ? "joining" : "off");
    if (state == 2 && em_cbor_get_str(info, (size_t)n, 2, EM_CBOR_TEXT, &text, &tn) == 0) { puts_(o, " ssid=\""); put(o, (const char *)text, tn); puts_(o, "\""); }
    if (state == 2 && em_cbor_get_str(info, (size_t)n, 3, EM_CBOR_TEXT, &text, &tn) == 0) { puts_(o, " ip="); put(o, (const char *)text, tn); }
    if (state == 2 && em_cbor_get_int(info, (size_t)n, 4, &rssi) == 0) { puts_(o, " rssi="); putn(o, rssi); }
    char ssid[40], psk[72];
    size_t sn = setting(h, "ssid", ssid, sizeof(ssid));
    puts_(o, " saved_ssid=\"");
    if (sn) put(o, ssid, sn); else puts_(o, "(flash network)");
    puts_(o, setting(h, "psk", psk, sizeof(psk)) ? "\" psk=set\n" : "\" psk=unset\n");
}

/* `loader start NAME`: the upload server, announcing that this device wants the package NAME; `loader stop` */
static void cmd_loader(const espmod_host *h, const tok_t *t, int count, out_t *o)
{
    if (!(h->features & ESPMOD_FEAT_LOADER)) { puts_(o, "error: this host has no loader\n"); return; }
    int rc;
    if (count == 3 && word_is(&t[1], "start")) rc = h->loader_start(h->user, (const uint8_t *)t[2].s, t[2].n);
    else if (count == 2 && word_is(&t[1], "stop")) rc = h->loader_stop(h->user);
    else { puts_(o, "error: loader start NAME | loader stop\n"); return; }
    if (rc == ESPMOD_ERR_BUSY) puts_(o, "error: busy (already started)\n");
    else if (rc) error_rc(o, "loader", rc);
    else puts_(o, "ok\n");
}

/* `NAME.METHOD key=value`: the request payload by the catalog, the module called, its answer as text */
static void cmd_call(const espmod_host *h, const uint8_t *line, size_t n, out_t *o)
{
    uint8_t *req = h->alloc(h->user, RESP_MAX, 4);
    uint8_t *ans = h->alloc(h->user, RESP_MAX, 4);
    uint8_t *tmp = h->alloc(h->user, RESP_MAX, 4);
    if (!req || !ans || !tmp) { puts_(o, "error: out of memory\n"); return; }
    em_cbor_w w;
    em_cbor_init(&w, req, RESP_MAX);
    do_parse(h, line, n, &w);
    int len = em_cbor_done(&w);
    const uint8_t *text; size_t tn;
    if (len <= 0) { puts_(o, "error: bad request\n"); return; }
    if (em_cbor_get_str(req, (size_t)len, 12, EM_CBOR_TEXT, &text, &tn) == 0) {     /* answered by the parser */
        put(o, (const char *)text, tn);
        if (tn) put(o, "\n", 1);
        return;
    }
    int64_t comp = 0, meth = 0;
    const uint8_t *fields; size_t fn;
    if (em_cbor_get_int(req, (size_t)len, 1, &comp) || em_cbor_get_int(req, (size_t)len, 2, &meth) ||
        raw_value(req, (size_t)len, 5, &fields, &fn)) { puts_(o, "error: bad request\n"); return; }
    size_t an = 0;
    int rc = (h->features & ESPMOD_FEAT_CALL) ? h->call(h->user, (int)comp, (int)meth, fields, fn, ans, RESP_MAX, &an) : ESPMOD_ERR_UNSUPPORTED;

    /* the answer for render: {1: component, 2: method, 6: result} or {7: error} */
    em_cbor_init(&w, tmp, RESP_MAX);
    em_cbor_map(&w, 3);
    em_cbor_uint(&w, 1); em_cbor_uint(&w, (uint64_t)comp);
    em_cbor_uint(&w, 2); em_cbor_uint(&w, (uint64_t)meth);
    if (rc == 0 || rc == ESPMOD_ERR_SIZE) { em_cbor_uint(&w, 6); if (an) em_cbor_put(&w, ans, an); else em_cbor_map(&w, 0); }
    else {
        static const char notfound[] = "module not running (start it)";
        em_cbor_uint(&w, 7);
        em_cbor_text(&w, rc == ESPMOD_ERR_NOTFOUND ? notfound : "request failed", rc == ESPMOD_ERR_NOTFOUND ? sizeof(notfound) - 1 : 14);
    }
    int tl = em_cbor_done(&w);
    uint8_t *out = h->alloc(h->user, RESP_MAX, 4);
    if (tl <= 0 || !out) { puts_(o, "error: answer too large\n"); return; }
    em_cbor_w r;
    em_cbor_init(&r, out, RESP_MAX);
    do_render(h, tmp, (size_t)tl, &r);
    int rl = em_cbor_done(&r);
    if (rl > 0 && em_cbor_get_str(out, (size_t)rl, 1, EM_CBOR_TEXT, &text, &tn) == 0) put(o, (const char *)text, tn);
}

/* One line in, the text answer out (o). */
static void do_line(const espmod_host *h, const uint8_t *line_in, size_t n, out_t *o)
{
    char *line = h->alloc(h->user, n + 1, 1);
    tok_t *t = h->alloc(h->user, sizeof(tok_t) * MAX_TOKENS, 8);
    if (!line || !t) { puts_(o, "error: out of memory\n"); return; }
    em_copy(line, line_in, n);
    int count = tokenize(line, n, t);
    if (count < 0) { puts_(o, "error: too many words\n"); return; }
    if (count == 0) return;
    uint8_t *buf = h->alloc(h->user, RESP_MAX, 4);
    if (!buf) { puts_(o, "error: out of memory\n"); return; }
    int len;
    if (word_is(&t[0], "help")) {
        help_text(o);
    } else if (word_is(&t[0], "wifi")) {
        cmd_wifi(h, t, count, o);
    } else if (word_is(&t[0], "loader")) {
        cmd_loader(h, t, count, o);
    } else if (word_is(&t[0], "status") || word_is(&t[0], "events") || word_is(&t[0], "reset") ||
               word_is(&t[0], "start") || word_is(&t[0], "stop")) {
        if (!(h->features & ESPMOD_FEAT_MODULES)) { puts_(o, "error: this host has no module control\n"); return; }
        if (word_is(&t[0], "status")) {
            len = h->modules_info(h->user, buf, RESP_MAX);
            if (len > 0) render_list(o, buf, (size_t)len, "slot");
        } else if (word_is(&t[0], "events")) {
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
                char *rest = h->alloc(h->user, n + 1, 1);
                if (!rest) { puts_(o, "error: out of memory\n"); return; }
                size_t rn = 0;
                for (int j = 2; j < count; j++) {
                    if (j > 2) rest[rn++] = ' ';
                    em_copy(rest + rn, t[j].s, t[j].n);
                    rn += t[j].n;
                }
                rc = h->module_start(h->user, (const uint8_t *)t[1].s, t[1].n, (const uint8_t *)rest, rn);
            }
            if (rc == ESPMOD_ERR_NOTFOUND) puts_(o, "error: unknown module\n");
            else if (rc) error_rc(o, word_is(&t[0], "stop") ? "stop" : "start", rc);
            else puts_(o, "ok\n");
        }
    } else {
        cmd_call(h, line_in, n, o);
    }
    if (!o->len) puts_(o, "ok\n");
    else if (o->p[o->len - 1] != '\n') put(o, "\n", 1);
}

ESPMOD_HANDLER(console)(const espmod_host *h, void *state, const espmod_call *c)
{
    (void)state;
    if (c->component == ESPMOD_HOST_COMPONENT) {
        if (c->method == ESPMOD_HOST_START && (h->features & ESPMOD_FEAT_SERIAL)) (void)h->serial_open(h->user, C_CONSOLE, M_LINE);
        else if (c->method == ESPMOD_HOST_STOP && (h->features & ESPMOD_FEAT_SERIAL)) (void)h->serial_close(h->user);
        return ESPMOD_RET_CONTINUE;
    }
    if (c->component != C_CONSOLE || !(c->flags & 1)) return ESPMOD_RET_CONTINUE;
    if (c->method == M_LINE) {
        char *text = h->alloc(h->user, OUT_MAX, 1);
        if (!text) return ESPMOD_RET_CONTINUE;
        out_t o = {text, OUT_MAX, 0};
        do_line(h, c->data, c->len, &o);
        h->respond(h->user, (const uint8_t *)o.p, o.len);
        return ESPMOD_RET_CONTINUE;
    }
    uint8_t *buf = h->alloc(h->user, RESP_MAX, 4);
    if (!buf) return ESPMOD_RET_CONTINUE;
    em_cbor_w w;
    em_cbor_init(&w, buf, RESP_MAX);
    if (c->method == M_PARSE) {
        const uint8_t *line; size_t n;
        if (em_cbor_get_str(c->data, c->len, 1, EM_CBOR_TEXT, &line, &n) == 0) do_parse(h, line, n, &w);
        else local(h, &w, "error: no line");
    } else if (c->method == M_RENDER) {
        do_render(h, c->data, c->len, &w);
    } else {
        return ESPMOD_RET_CONTINUE;
    }
    int n = em_cbor_done(&w);
    if (n > 0) h->respond(h->user, buf, (size_t)n);
    return ESPMOD_RET_CONTINUE;
}
