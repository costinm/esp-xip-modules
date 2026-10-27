/* SPDX-License-Identifier: Apache-2.0 */
/* The pure part of the common QUIC receive function: see include/xip_quic.h. */
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include "xip.h"
#include "xip_cbor.h"
#include "xip_quic.h"

unsigned xip_quic_cid_class_len(uint8_t first)
{
    if (!(first & 0x80)) return 1;
    switch ((first >> 5) & 3) {
    case 0: return 2;
    case 1: return 4;
    case 2: return 8;
    default: return 0;           /* 111: reserved */
    }
}

xip_quic_class xip_quic_classify(const uint8_t *p, size_t len, unsigned short_cid_len, xip_quic_hdr *h)
{
    if (!p || !len || !(p[0] & 0x40)) return XIP_QUIC_NOT_QUIC;       /* the fixed bit is shared by both forms */
    memset(h, 0, sizeof(*h));
    h->first = p[0];
    if (!(p[0] & 0x80)) {
        const unsigned n = short_cid_len ? short_cid_len : (len > 1 ? xip_quic_cid_class_len(p[1]) : 0);
        const unsigned pn = (p[0] & 3u) + 1;
        if (!n || n > XIP_QUIC_CID_MAX || len < 1 + n + pn) return XIP_QUIC_MALFORMED;
        h->dcid = p + 1; h->dcid_len = (uint8_t)n;
        h->body = (uint16_t)(1 + n);
        return XIP_QUIC_SHORT;
    }
    /* RFC 8999 invariants: first byte, version, DCID length + DCID, SCID length + SCID. */
    if (len < 7) return XIP_QUIC_MALFORMED;
    h->is_long = true;
    h->version = (uint32_t)p[1] << 24 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 8 | p[4];
    size_t at = 5;
    const uint8_t dl = p[at++];
    if (dl > XIP_QUIC_CID_MAX || len < at + dl + 1) return XIP_QUIC_MALFORMED;
    h->dcid = p + at; h->dcid_len = dl; at += dl;
    const uint8_t sl = p[at++];
    if (sl > XIP_QUIC_CID_MAX || len < at + sl) return XIP_QUIC_MALFORMED;
    h->scid = p + at; h->scid_len = sl; at += sl;
    h->body = (uint16_t)at;
    return XIP_QUIC_LONG;
}

size_t xip_quic_long_build(uint8_t *out, size_t cap, uint32_t version, const uint8_t *dcid, unsigned dcid_len, const uint8_t *scid, unsigned scid_len)
{
    if (dcid_len > XIP_QUIC_CID_MAX || scid_len > XIP_QUIC_CID_MAX) return 0;
    const size_t need = 1 + 4 + 1 + dcid_len + 1 + scid_len;
    if (cap < need) return 0;
    size_t at = 0;
    out[at++] = 0xc0;
    out[at++] = (uint8_t)(version >> 24); out[at++] = (uint8_t)(version >> 16); out[at++] = (uint8_t)(version >> 8); out[at++] = (uint8_t)version;
    out[at++] = (uint8_t)dcid_len;
    if (dcid_len) { memcpy(out + at, dcid, dcid_len); at += dcid_len; }
    out[at++] = (uint8_t)scid_len;
    if (scid_len) { memcpy(out + at, scid, scid_len); at += scid_len; }
    return at;
}

int xip_quic_envelope(const uint8_t *cbor, size_t len, xip_quic_env *e)
{
    int64_t c, m, id = 0;
    memset(e, 0, sizeof(*e));
    if (xip_cbor_get_int(cbor, len, 1, &c) != 0 || xip_cbor_get_int(cbor, len, 2, &m) != 0) return -1;
    if (c <= 0 || c > INT32_MAX || m <= 0 || m > INT32_MAX) return -1;
    const int rc = xip_cbor_get_int(cbor, len, 3, &id);
    if (rc < 0 || (rc == 0 && id < 0)) return -1;
    e->component = (int32_t)c; e->method = (int32_t)m;
    e->has_id = rc == 0; e->id = rc == 0 ? (uint64_t)id : 0;
    xip_cbor_r v;
    e->response = xip_cbor_get(cbor, len, 6, &v) == 0 || xip_cbor_get(cbor, len, 7, &v) == 0;
    return 0;
}

/* ---- the table: sorted by (length, bytes) so a lookup is a binary search ---- */
static int cmp(const xip_quic_entry *a, const uint8_t *cid, unsigned len)
{
    if (a->len != len) return a->len < len ? -1 : 1;
    return memcmp(a->cid, cid, len);
}
static unsigned lower_bound(const xip_quic_table *t, const uint8_t *cid, unsigned len)
{
    unsigned lo = 0, hi = t->n;
    while (lo < hi) {
        const unsigned mid = (lo + hi) / 2;
        if (cmp(&t->e[mid], cid, len) < 0) lo = mid + 1; else hi = mid;
    }
    return lo;
}
const xip_quic_entry *xip_quic_table_find(const xip_quic_table *t, const uint8_t *cid, unsigned len)
{
    if (!t || !cid || !len || len > XIP_QUIC_CID_MAX || !t->n) return NULL;
    const unsigned at = lower_bound(t, cid, len);
    return at < t->n && cmp(&t->e[at], cid, len) == 0 ? &t->e[at] : NULL;
}
int xip_quic_table_add(xip_quic_table *t, const uint8_t *cid, unsigned len, unsigned kind, uint32_t egress, uint64_t token,
                       const uint8_t *out_cid, unsigned out_len)
{
    if (!t || !cid || !len || len > XIP_QUIC_CID_MAX || (kind != XIP_QUIC_DCID_LOCAL && kind != XIP_QUIC_DCID_RELAY)) return XIP_ERR_ARG;
    if (kind == XIP_QUIC_DCID_RELAY && (!out_cid || !out_len || out_len > XIP_QUIC_CID_MAX)) return XIP_ERR_ARG;
    unsigned at = lower_bound(t, cid, len);
    const bool exists = at < t->n && cmp(&t->e[at], cid, len) == 0;
    if (!exists) {
        if (t->n >= XIP_QUIC_TABLE_MAX) return XIP_ERR_NOMEM;
        if (t->n == t->cap) {                                  /* grows: 4, 8, 16, ... */
            const unsigned cap = t->cap ? t->cap * 2u : 4u;
            xip_quic_entry *bigger = realloc(t->e, cap * sizeof(*bigger));
            if (!bigger) return XIP_ERR_NOMEM;
            t->e = bigger; t->cap = (uint16_t)cap;
        }
        memmove(&t->e[at + 1], &t->e[at], (t->n - at) * sizeof(t->e[0]));
        t->n++;
        memset(&t->e[at], 0, sizeof(t->e[0]));
        memcpy(t->e[at].cid, cid, len);
        t->e[at].len = (uint8_t)len;
    }
    xip_quic_entry *e = &t->e[at];
    free(e->route); e->route = NULL; e->owner = e->cursor = 0; e->out_address_len=0;
    e->kind = (uint8_t)kind; e->egress = egress; e->token = token;
    e->out_len = 0;
    if (kind == XIP_QUIC_DCID_RELAY) { memcpy(e->out_cid, out_cid, out_len); e->out_len = (uint8_t)out_len; }
    return XIP_OK;
}
unsigned xip_quic_cid_encode(uint64_t value, uint8_t *out)
{
    if (!value || value > ((uint64_t)1 << 61) - 1) return 0;
    unsigned n = value <= 0x7f ? 1 : value <= 0x1fff ? 2 : value <= 0x1fffffff ? 4 : 8;
    for (unsigned i = 0; i < n; i++) out[i] = (uint8_t)(value >> (8 * (n - i - 1)));
    const uint8_t tag = (uint8_t)((n == 1 ? 0 : n == 2 ? 0x4 : n == 4 ? 0x5 : 0x6) << 5);
    out[0] = (uint8_t)((out[0] & (n == 1 ? 0x7f : 0x1f)) | tag);
    return n;
}
int xip_quic_table_alloc(xip_quic_table *t, unsigned kind, uint32_t egress, uint64_t token, const uint8_t *out_cid, unsigned out_len, uint8_t *cid, unsigned *len)
{
    if (!t || !cid || !len) return XIP_ERR_ARG;
    if (t->n >= XIP_QUIC_TABLE_MAX) return XIP_ERR_NOMEM;
    for (uint64_t v = 1; v <= (uint64_t)XIP_QUIC_TABLE_MAX + 1; v++) {      /* the lowest free value: at most n + 1 tries */
        uint8_t buf[8];
        const unsigned n = xip_quic_cid_encode(v, buf);
        if (xip_quic_table_find(t, buf, n)) continue;
        const int rc = xip_quic_table_add(t, buf, n, kind, egress, token, out_cid, out_len);
        if (rc) return rc;
        memcpy(cid, buf, n); *len = n;
        return XIP_OK;
    }
    return XIP_ERR_NOMEM;
}
int xip_quic_short_rewrite(uint8_t *p, size_t len, const xip_quic_hdr *h, const uint8_t *ncid, unsigned nlen, size_t room, int *shift)
{
    if (!p || !h || h->is_long || !ncid || !nlen || nlen > XIP_QUIC_CID_MAX || !shift) return XIP_ERR_ARG;
    const int d = (int)h->dcid_len - (int)nlen;                  /* the packet starts d bytes later */
    if (d < 0 && (size_t)(-d) > room) return XIP_ERR_SIZE;
    if (len - (size_t)(d > 0 ? d : 0) + (size_t)(d < 0 ? -d : 0) > XIP_QUIC_MAX_PACKET) return XIP_ERR_SIZE;
    uint8_t *q = p + d;
    const uint8_t first = h->first;
    q[0] = first;
    memmove(q + 1, ncid, nlen);
    *shift = d;
    return XIP_OK;
}
int xip_quic_table_remove(xip_quic_table *t, const uint8_t *cid, unsigned len)
{
    if (!t || !cid || !len || len > XIP_QUIC_CID_MAX) return XIP_ERR_ARG;
    const unsigned at = lower_bound(t, cid, len);
    if (at >= t->n || cmp(&t->e[at], cid, len)) return XIP_ERR_NOTFOUND;
    free(t->e[at].route);
    memmove(&t->e[at], &t->e[at + 1], (t->n - at - 1) * sizeof(t->e[0]));
    t->n--;
    if(!t->n){free(t->e);t->e=NULL;t->cap=0;}
    return XIP_OK;
}
void xip_quic_table_clear(xip_quic_table *t)
{
    if (!t) return;
    for (unsigned i=0;i<t->n;i++) free(t->e[i].route);
    free(t->e);
    memset(t, 0, sizeof(*t));
}

/* ---- replies ---- */
static const xip_quic_reply *find_note(const xip_quic_replies *t, uint32_t link, uint64_t source, uint64_t id)
{
    for (unsigned i = 0; i < XIP_QUIC_REPLIES; i++)
        if (t->r[i].used && t->r[i].link == link && t->r[i].source == source && t->r[i].id == id) return &t->r[i];
    return NULL;
}
void xip_quic_reply_note(xip_quic_replies *t, uint32_t link, uint64_t source, uint64_t id, const xip_quic_hdr *rq, uint32_t now_ms)
{
    if (rq->dcid_len > XIP_QUIC_CID_MAX || rq->scid_len > XIP_QUIC_CID_MAX) return;
    xip_quic_reply *r = (xip_quic_reply *)find_note(t, link, source, id);
    for (unsigned i = 0; i < XIP_QUIC_REPLIES && !r; i++) if (!t->r[i].used) r = &t->r[i];
    for (unsigned i = 0; i < XIP_QUIC_REPLIES && !r; i++)           /* then an expired note */
        if ((uint32_t)(now_ms - t->r[i].at_ms) > XIP_QUIC_REPLY_MS) r = &t->r[i];
    if (!r) { r = &t->r[t->next]; t->next = (uint8_t)((t->next + 1) % XIP_QUIC_REPLIES); }     /* full: the oldest slot in turn */
    memset(r, 0, sizeof(*r));
    r->used = true; r->link = link; r->source = source; r->id = id; r->at_ms = now_ms;
    r->dcid_len = rq->scid_len; if (rq->scid_len) memcpy(r->dcid, rq->scid, rq->scid_len);
    r->scid_len = rq->dcid_len; if (rq->dcid_len) memcpy(r->scid, rq->dcid, rq->dcid_len);
}
const xip_quic_reply *xip_quic_reply_find(const xip_quic_replies *t, uint32_t link, uint64_t source, uint64_t id, uint32_t now_ms)
{
    const xip_quic_reply *r = find_note(t, link, source, id);
    return r && (uint32_t)(now_ms - r->at_ms) <= XIP_QUIC_REPLY_MS ? r : NULL;
}
void xip_quic_reply_done(xip_quic_replies *t, const xip_quic_reply *r)
{
    if (r >= t->r && r < t->r + XIP_QUIC_REPLIES) ((xip_quic_reply *)r)->used = false;
}

int xip_quic_relay_set(xip_quic_table *t,uint32_t owner,const xip_relay_route *r,unsigned mode)
{
    if(!t || !owner || !r || mode>1 || !r->dcid_len || r->dcid_len>XIP_QUIC_CID_MAX ||
       !r->hop_count || r->hop_count>4 || r->reserved || r->endpoint>1)return XIP_ERR_ARG;
    if(r->endpoint && (r->dcid_len>8 || (r->dcid_len==1 && !r->dcid[0]) || xip_quic_cid_class_len(r->dcid[0])!=r->dcid_len))return XIP_ERR_ARG;
    unsigned total=0;
    for(unsigned i=0;i<r->hop_count;i++){
        const xip_relay_hop *h=&r->hops[i];
        if(!h->link || !h->cid_len || h->cid_len>XIP_QUIC_CID_MAX || (h->address_len!=0 && h->address_len!=22) ||
           memcmp(h->padding,(uint8_t[4]){0},4))return XIP_ERR_ARG;
        if(h->address_len==22 && ((!h->address[16] && !h->address[17]) || h->address[22] || h->address[23]))return XIP_ERR_ARG;
        total+=h->weight;
    }
    if(!total)return XIP_ERR_ARG;
    const xip_quic_entry *old=xip_quic_table_find(t,r->dcid,r->dcid_len);
    if(old && (old->owner!=owner || mode==0))return XIP_ERR_BUSY;
    if(!old && mode==1)return XIP_ERR_NOTFOUND;
    xip_relay_route *copy=malloc(sizeof(*copy));if(!copy)return XIP_ERR_NOMEM;
    *copy=*r;
    uint64_t last=old?old->last_used_us:0;uint32_t idle=old?old->idle_ms:0;
    const xip_relay_hop *h=&r->hops[0];
    int rc=xip_quic_table_add(t,r->dcid,r->dcid_len,XIP_QUIC_DCID_RELAY,h->link,h->token,h->rewritten_dcid,h->cid_len);
    if(rc){free(copy);return rc;}
    xip_quic_entry *e=&t->e[lower_bound(t,r->dcid,r->dcid_len)];
    e->route=copy;e->owner=owner;e->last_used_us=last;e->idle_ms=idle;return 0;
}
int xip_quic_relay_get(const xip_quic_table *t,uint32_t owner,unsigned index,xip_relay_route *out)
{
    if(!out)return XIP_ERR_ARG;
    for(unsigned i=0;i<t->n;i++)if(t->e[i].owner==owner && t->e[i].route){
        if(!index--){*out=*t->e[i].route;return 0;}
    }
    return XIP_ERR_NOTFOUND;
}
int xip_quic_relay_remove(xip_quic_table *t,uint32_t owner,const uint8_t *cid,unsigned len)
{
    const xip_quic_entry *e=xip_quic_table_find(t,cid,len);
    if(!e)return XIP_ERR_NOTFOUND;
    if(e->owner!=owner)return XIP_ERR_BUSY;
    return xip_quic_table_remove(t,cid,len);
}
void xip_quic_relay_release(xip_quic_table *t,uint32_t owner)
{
    for(unsigned i=t->n;i>0;i--)if(t->e[i-1].owner==owner)
        xip_quic_table_remove(t,t->e[i-1].cid,t->e[i-1].len);
}
int xip_quic_relay_select_at(xip_quic_table *t,const uint8_t *cid,unsigned len,uint64_t now_us,xip_quic_entry *out)
{
    const xip_quic_entry *found=xip_quic_table_find(t,cid,len);
    if(!found)return XIP_ERR_NOTFOUND;
    xip_quic_entry *e=&t->e[found-t->e];
    if(e->route && e->idle_ms && now_us>=e->last_used_us && now_us-e->last_used_us>=(uint64_t)e->idle_ms*1000)return XIP_ERR_NOTFOUND;
    if(e->route && now_us>e->last_used_us)e->last_used_us=now_us;
    *out=*e;
    if(!e->route)return 0;
    if(e->route->endpoint)out->kind=3;
    unsigned total=0;for(unsigned i=0;i<e->route->hop_count;i++)total+=e->route->hops[i].weight;
    unsigned at=e->cursor++%total;
    for(unsigned i=0;i<e->route->hop_count;i++){
        const xip_relay_hop *h=&e->route->hops[i];
        if(at<h->weight){out->egress=h->link;out->token=h->token;out->out_len=h->cid_len;
            memcpy(out->out_cid,h->rewritten_dcid,h->cid_len);
            out->out_address_len=h->address_len;memcpy(out->out_address,h->address,24);return 0;}
        at-=h->weight;
    }
    return XIP_ERR_ARG;
}

int xip_quic_relay_select(xip_quic_table *t,const uint8_t *cid,unsigned len,xip_quic_entry *out)
{return xip_quic_relay_select_at(t,cid,len,0,out);}
int xip_quic_relay_stats(const xip_quic_table *t,uint32_t owner,const uint8_t *cid,unsigned len,xip_relay_stats *out)
{
    if(!out)return XIP_ERR_ARG;
    const xip_quic_entry *e=xip_quic_table_find(t,cid,len);
    if(!e || e->owner!=owner || !e->route)return XIP_ERR_NOTFOUND;
    *out=(xip_relay_stats){.last_used_us=e->last_used_us,.idle_ms=e->idle_ms};return 0;
}
int xip_quic_relay_expire(xip_quic_table *t,uint32_t owner,uint64_t now_us)
{
    int count=0;
    for(unsigned i=t->n;i>0;i--){
        xip_quic_entry *e=&t->e[i-1];
        if(e->route && (!owner || e->owner==owner) && e->idle_ms && now_us>=e->last_used_us && now_us-e->last_used_us>=(uint64_t)e->idle_ms*1000){
            xip_quic_table_remove(t,e->cid,e->len);count++;
        }
    }
    return count;
}
int xip_quic_relay_idle(xip_quic_table *t,uint32_t owner,const uint8_t *cid,unsigned len,uint32_t idle_ms)
{
    const xip_quic_entry *found=xip_quic_table_find(t,cid,len);
    if(!found || found->owner!=owner || !found->route)return XIP_ERR_NOTFOUND;
    t->e[found-t->e].idle_ms=idle_ms;return 0;
}
int xip_quic_relay_create(xip_quic_table *t,uint32_t owner,xip_relay_route *route,uint32_t idle_ms,bool reuse,uint64_t now_us)
{
    if(!t || !owner || !route || route->dcid_len || route->hop_count!=1)return XIP_ERR_ARG;
    xip_quic_relay_expire(t,owner,now_us);
    if(reuse)for(unsigned i=0;i<t->n;i++){
        xip_quic_entry *e=&t->e[i];
        if(e->owner==owner && e->route && e->idle_ms==idle_ms && e->route->endpoint==route->endpoint &&
           e->route->hop_count==1 && !memcmp(&e->route->hops[0],&route->hops[0],sizeof(route->hops[0]))){
            *route=*e->route;e->last_used_us=now_us;return 1;
        }
    }
    /* A separate high range avoids the local endpoint's small sequential CIDs.
     * Never recycle a closed ID during this runtime lifetime: delayed packets
     * cannot become traffic for a newly allocated circuit. */
    for(unsigned tries=0;tries<=XIP_QUIC_TABLE_MAX;tries++){
        if(t->next_relay_cid>=((uint64_t)1<<60)-1)return XIP_ERR_NOMEM;
        xip_relay_route candidate=*route;
        candidate.dcid_len=xip_quic_cid_encode(((uint64_t)1<<60)|++t->next_relay_cid,candidate.dcid);
        if(xip_quic_table_find(t,candidate.dcid,candidate.dcid_len))continue;
        int rc=xip_quic_relay_set(t,owner,&candidate,0);if(rc)return rc;
        xip_quic_entry *e=&t->e[lower_bound(t,candidate.dcid,candidate.dcid_len)];
        e->last_used_us=now_us;e->idle_ms=idle_ms;*route=candidate;return 0;
    }
    return XIP_ERR_NOMEM;
}
