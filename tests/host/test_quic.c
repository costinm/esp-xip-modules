/* The pure part of the common QUIC receive function (components/xip_runtime/src/xip_quic.c): header classification, the relay table, the
 * envelope fields and the reply notes. The golden packets are built by hand from the layouts of quic-lite (/ws/dmesh/crates/quic-lite,
 * lib.rs encode_long_packet, ConnectionId::encode, ShortHeader::encode), which is only read. */
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <stdbool.h>
#include "xip.h"
#include "xip_quic.h"

static void test_classify(void)
{
    xip_quic_hdr h;
    /* quic-lite Initial: 0xc0 | type 0 | pn_len-1 = 0 -> 0xc0; version 444d0001; DCID len 1 (CID 0x2a: class 0); SCID len 2 (CID 0x1234: 100 tag -> 0x92 0x34);
     * token len 0; length varint 3 (1 pn byte + 2 payload); pn 7; payload */
    const uint8_t initial[] = {0xc0, 0x44, 0x4d, 0x00, 0x01, 0x01, 0x2a, 0x02, 0x92, 0x34, 0x00, 0x03, 0x07, 0xaa, 0xbb};
    assert(xip_quic_classify(initial, sizeof initial, 0, &h) == XIP_QUIC_LONG);
    assert(h.is_long && h.version == XIP_QUIC_VERSION_DMESH && h.dcid_len == 1 && h.dcid[0] == 0x2a && h.scid_len == 2 && h.scid[0] == 0x92 && h.scid[1] == 0x34);
    assert(h.body == 10);
    /* quic-lite PathOpen (type 1): first 0xd0 ; no CIDs at all (lengths 0) is valid for a long header */
    const uint8_t path_open[] = {0xd0, 0x44, 0x4d, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x55};
    assert(xip_quic_classify(path_open, sizeof path_open, 0, &h) == XIP_QUIC_LONG && h.dcid_len == 0 && h.scid_len == 0 && h.body == 7);
    /* short: flags 0x41 (fixed bit, pn_len 2), DCID value 0x00012345 -> 4 bytes, tag 101: 0xa0 0x01 0x23 0x45, pn 2 bytes, payload */
    const uint8_t sh4[] = {0x41, 0xa0, 0x01, 0x23, 0x45, 0x00, 0x09, 0xde, 0xad};
    assert(xip_quic_classify(sh4, sizeof sh4, 0, &h) == XIP_QUIC_SHORT);
    assert(!h.is_long && h.dcid_len == 4 && !memcmp(h.dcid, sh4 + 1, 4) && h.body == 5);
    /* class rule: 1, 2, 4 and 8 bytes; 111 reserved */
    assert(xip_quic_cid_class_len(0x05) == 1 && xip_quic_cid_class_len(0x92) == 2 && xip_quic_cid_class_len(0xa0) == 4 && xip_quic_cid_class_len(0xc0) == 8 && xip_quic_cid_class_len(0xe0) == 0);
    const uint8_t sh1[] = {0x40, 0x05, 0x99};
    assert(xip_quic_classify(sh1, sizeof sh1, 0, &h) == XIP_QUIC_SHORT && h.dcid_len == 1 && h.body == 2);
    const uint8_t sh8[] = {0x43, 0xc0, 1, 2, 3, 4, 5, 6, 7, 1, 2, 3, 4};
    assert(xip_quic_classify(sh8, sizeof sh8, 0, &h) == XIP_QUIC_SHORT && h.dcid_len == 8);
    /* a fixed length overrides the class rule */
    assert(xip_quic_classify(sh8, sizeof sh8, 3, &h) == XIP_QUIC_SHORT && h.dcid_len == 3 && h.body == 4);
    /* not QUIC: empty, fixed bit clear (either form) */
    const uint8_t plain[] = {0x00, 1, 2, 3}, nofix[] = {0x80, 0, 0, 0, 1, 0, 0};
    assert(xip_quic_classify(plain, sizeof plain, 0, &h) == XIP_QUIC_NOT_QUIC && xip_quic_classify(nofix, sizeof nofix, 0, &h) == XIP_QUIC_NOT_QUIC);
    assert(xip_quic_classify(plain, 0, 0, &h) == XIP_QUIC_NOT_QUIC && xip_quic_classify(NULL, 4, 0, &h) == XIP_QUIC_NOT_QUIC);
    /* malformed: every truncation of the long and short packets is a clean answer, never a read past the end */
    for (size_t n = 1; n < 10; n++) {
        const xip_quic_class c = xip_quic_classify(initial, n, 0, &h);
        assert(c == XIP_QUIC_MALFORMED || (c == XIP_QUIC_LONG && (size_t)h.body <= n));
    }
    for (size_t n = 1; n < sizeof sh4; n++) {
        const xip_quic_class c = xip_quic_classify(sh4, n, 0, &h);
        assert(c == XIP_QUIC_MALFORMED || (c == XIP_QUIC_SHORT && (size_t)h.body + 2 <= n));
    }
    const uint8_t reserved[] = {0x40, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8};
    assert(xip_quic_classify(reserved, sizeof reserved, 0, &h) == XIP_QUIC_MALFORMED);
    const uint8_t longcid[] = {0xc0, 0x44, 0x4d, 0x00, 0x02, 21, 0, 0};          /* DCID length 21 > 20 */
    assert(xip_quic_classify(longcid, sizeof longcid, 0, &h) == XIP_QUIC_MALFORMED);
    const uint8_t lies[] = {0xc0, 0x44, 0x4d, 0x00, 0x02, 4, 1, 2};               /* DCID length beyond the packet */
    assert(xip_quic_classify(lies, sizeof lies, 0, &h) == XIP_QUIC_MALFORMED);
}

static void test_build_and_envelope(void)
{
    uint8_t out[64];
    const uint8_t d[] = {0xa0, 1, 2, 3}, s[] = {0x2a};
    const size_t n = xip_quic_long_build(out, sizeof out, XIP_QUIC_VERSION_CBOR, d, 4, s, 1);
    const uint8_t want[] = {0xc0, 0x44, 0x4d, 0x00, 0x02, 4, 0xa0, 1, 2, 3, 1, 0x2a};
    assert(n == sizeof want && !memcmp(out, want, n));
    assert(xip_quic_long_build(out, 8, XIP_QUIC_VERSION_CBOR, d, 4, s, 1) == 0);
    assert(xip_quic_long_build(out, sizeof out, 1, d, 21, s, 1) == 0);
    xip_quic_hdr h;
    uint8_t pkt[80];
    memcpy(pkt, want, n);
    /* {1: 49, 2: 5, 3: 987, 5: {}} */
    const uint8_t env[] = {0xa4, 0x01, 0x18, 0x31, 0x02, 0x05, 0x03, 0x19, 0x03, 0xdb, 0x05, 0xa0};
    memcpy(pkt + n, env, sizeof env);
    assert(xip_quic_classify(pkt, n + sizeof env, 0, &h) == XIP_QUIC_LONG && h.version == XIP_QUIC_VERSION_CBOR && h.body == n);
    xip_quic_env e;
    assert(xip_quic_envelope(pkt + h.body, sizeof env, &e) == 0 && e.component == 49 && e.method == 5 && e.has_id && e.id == 987);
    const uint8_t noid[] = {0xa2, 0x01, 0x14, 0x02, 0x05};
    assert(xip_quic_envelope(noid, sizeof noid, &e) == 0 && !e.has_id && e.id == 0);
    const uint8_t answer[] = {0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x01, 0x06, 0xa0}, failure[] = {0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x01, 0x07, 0x21};
    assert(xip_quic_envelope(answer, sizeof answer, &e) == 0 && e.response && xip_quic_envelope(failure, sizeof failure, &e) == 0 && e.response);
    assert(xip_quic_envelope(env, sizeof env, &e) == 0 && !e.response);
    const uint8_t nomethod[] = {0xa1, 0x01, 0x14}, notmap[] = {0x81, 0x01}, textid[] = {0xa3, 1, 0x14, 2, 5, 3, 0x61, 'x'}, zero[] = {0xa2, 1, 0, 2, 5};
    assert(xip_quic_envelope(nomethod, sizeof nomethod, &e) != 0 && xip_quic_envelope(notmap, sizeof notmap, &e) != 0);
    assert(xip_quic_envelope(textid, sizeof textid, &e) != 0 && xip_quic_envelope(zero, sizeof zero, &e) != 0);
    for (size_t k = 0; k < sizeof env; k++) (void)xip_quic_envelope(env, k, &e);     /* truncations: no read past the end (ASan) */
}

static void test_table(void)
{
    xip_quic_table t = {0};
    const uint8_t out[] = {0x05};
    uint8_t cid[4];
    for (unsigned i = 0; i < 40; i++) {
        cid[0] = 0xa0; cid[1] = 0; cid[2] = (uint8_t)(i * 7); cid[3] = (uint8_t)i;
        assert(xip_quic_table_add(&t, cid, 4, i % 2 ? XIP_QUIC_DCID_RELAY : XIP_QUIC_DCID_LOCAL, i, i * 1000ull, out, 1) == 0);
    }
    assert(t.n == 40 && t.cap >= 40);
    for (unsigned i = 0; i < 40; i++) {
        cid[2] = (uint8_t)(i * 7); cid[3] = (uint8_t)i; cid[0] = 0xa0; cid[1] = 0;
        const xip_quic_entry *e = xip_quic_table_find(&t, cid, 4);
        assert(e && e->egress == i && e->kind == (i % 2 ? XIP_QUIC_DCID_RELAY : XIP_QUIC_DCID_LOCAL) && e->token == i * 1000ull);
        assert(e->out_len == (i % 2 ? 1 : 0));
    }
    const uint8_t other[] = {0xa0, 0, 0xff, 0xff};
    assert(!xip_quic_table_find(&t, other, 4) && !xip_quic_table_find(&t, other, 3));       /* same bytes, other length */
    assert(xip_quic_table_add(&t, cid, 4, XIP_QUIC_DCID_LOCAL, 99, 0, NULL, 0) == 0 && t.n == 40 && xip_quic_table_find(&t, cid, 4)->egress == 99);   /* replace */
    assert(xip_quic_table_add(&t, cid, 0, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0) == XIP_ERR_ARG && xip_quic_table_add(&t, cid, 21, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0) == XIP_ERR_ARG);
    assert(xip_quic_table_add(&t, cid, 4, 7, 0, 0, NULL, 0) == XIP_ERR_ARG && xip_quic_table_add(&t, cid, 4, XIP_QUIC_DCID_RELAY, 0, 0, NULL, 0) == XIP_ERR_ARG);
    assert(xip_quic_table_remove(&t, cid, 4) == 0 && !xip_quic_table_find(&t, cid, 4) && t.n == 39);
    assert(xip_quic_table_remove(&t, cid, 4) == XIP_ERR_NOTFOUND);
    xip_quic_table_clear(&t);
    assert(!t.e && !t.n);
    /* it stops at the limit */
    for (unsigned i = 0; i < XIP_QUIC_TABLE_MAX + 5; i++) {
        uint8_t k[2] = {(uint8_t)(i >> 8), (uint8_t)i};
        const int rc = xip_quic_table_add(&t, k, 2, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0);
        assert(i < XIP_QUIC_TABLE_MAX ? rc == 0 : rc == XIP_ERR_NOMEM);
    }
    xip_quic_table_clear(&t);
}

static void test_alloc(void)
{
    uint8_t b[8];
    /* quic-lite ConnectionId::encode: 1 byte up to 0x7f, 2 bytes (tag 100, 13 bits) up to 0x1fff, 4 bytes (tag 101), 8 bytes (tag 110) */
    assert(xip_quic_cid_encode(0, b) == 0 && xip_quic_cid_encode(1, b) == 1 && b[0] == 0x01 && xip_quic_cid_encode(0x7f, b) == 1 && b[0] == 0x7f);
    assert(xip_quic_cid_encode(0x80, b) == 2 && b[0] == 0x80 && b[1] == 0x80);
    assert(xip_quic_cid_encode(0x1fff, b) == 2 && b[0] == 0x9f && b[1] == 0xff);
    assert(xip_quic_cid_encode(0x2000, b) == 4 && b[0] == 0xa0 && b[1] == 0 && b[2] == 0x20 && b[3] == 0);
    assert(xip_quic_cid_encode(0x12345, b) == 4 && !memcmp(b, "\xa0\x01\x23\x45", 4));      /* the golden short header's CID */
    assert(xip_quic_cid_encode(0x20000000, b) == 8 && b[0] == 0xc0 && b[4] == 0x20);
    assert(xip_quic_cid_encode(1ull << 61, b) == 0);
    /* every encoded CID is read back by the classifier with the same length */
    for (uint64_t v = 1; v < 0x3000; v += 7) {
        const unsigned n = xip_quic_cid_encode(v, b);
        assert(xip_quic_cid_class_len(b[0]) == n);
    }
    xip_quic_table t = {0};
    uint8_t cid[XIP_QUIC_CID_MAX];
    unsigned len;
    const uint8_t out[] = {0x05};
    for (unsigned i = 1; i <= 16; i++) {                       /* 16 relays: values 1..16, one byte each */
        assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_RELAY, i, 0, out, 1, cid, &len) == 0 && len == 1 && cid[0] == i);
    }
    assert(t.n == 16 && t.cap == 16);
    assert(xip_quic_table_remove(&t, (const uint8_t[]){0x05}, 1) == 0);
    assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0, cid, &len) == 0 && len == 1 && cid[0] == 5);   /* the freed ID is reused */
    while (t.n < XIP_QUIC_TABLE_MAX) assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0, cid, &len) == 0);
    assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_LOCAL, 0, 0, NULL, 0, cid, &len) == XIP_ERR_NOMEM);
    assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_RELAY, 0, 0, NULL, 0, cid, &len) == XIP_ERR_NOMEM);
    xip_quic_table_clear(&t);
    assert(xip_quic_table_alloc(&t, XIP_QUIC_DCID_RELAY, 0, 0, NULL, 0, cid, &len) == XIP_ERR_ARG);     /* a relay needs its outgoing DCID */
    xip_quic_table_clear(&t);
}

static void test_rewrite(void)
{
    /* a short packet in a buffer with 8 bytes of empty headroom in front: DCID 0x12345 (4 bytes) -> 1 byte 0x2a, then back to 8 bytes */
    uint8_t buf[64] = {0};
    uint8_t *p = buf + 8;
    const uint8_t pkt[] = {0x41, 0xa0, 0x01, 0x23, 0x45, 0x00, 0x09, 0xde, 0xad};
    memcpy(p, pkt, sizeof pkt);
    xip_quic_hdr h;
    assert(xip_quic_classify(p, sizeof pkt, 0, &h) == XIP_QUIC_SHORT);
    int shift = 0;
    const uint8_t n1[] = {0x2a};
    assert(xip_quic_short_rewrite(p, sizeof pkt, &h, n1, 1, 8, &shift) == 0 && shift == 3);
    const uint8_t want1[] = {0x41, 0x2a, 0x00, 0x09, 0xde, 0xad};
    assert(!memcmp(p + shift, want1, sizeof want1));                        /* flags kept, packet number and body unmoved */
    assert(p[shift + 2] == pkt[5]);
    p += shift;
    assert(xip_quic_classify(p, sizeof want1, 0, &h) == XIP_QUIC_SHORT && h.dcid_len == 1);
    const uint8_t n8[] = {0xc0, 1, 2, 3, 4, 5, 6, 7};
    assert(xip_quic_short_rewrite(p, sizeof want1, &h, n8, 8, 5, &shift) == XIP_ERR_SIZE);    /* needs 7 bytes of room */
    assert(xip_quic_short_rewrite(p, sizeof want1, &h, n8, 8, 11, &shift) == 0 && shift == -7);
    const uint8_t want8[] = {0x41, 0xc0, 1, 2, 3, 4, 5, 6, 7, 0x00, 0x09, 0xde, 0xad};
    assert(!memcmp(p + shift, want8, sizeof want8));
    assert(xip_quic_short_rewrite(p, sizeof want1, &h, n8, 8, 11, &shift) == 0);   /* idempotent on a re-classified header is not needed; no crash */
}

static void test_replies(void)
{
    xip_quic_replies r = {0};
    const uint8_t d[] = {0xa0, 1, 2, 3}, s[] = {0x2a, 0x2b};
    xip_quic_hdr rq = {.is_long = true, .dcid = d, .dcid_len = 4, .scid = s, .scid_len = 2};
    xip_quic_reply_note(&r, 0x40000001, 7, 77, &rq, 1000);
    const xip_quic_reply *f = xip_quic_reply_find(&r, 0x40000001, 7, 77, 1500);
    assert(f && f->dcid_len == 2 && !memcmp(f->dcid, s, 2) && f->scid_len == 4 && !memcmp(f->scid, d, 4));      /* swapped */
    assert(!xip_quic_reply_find(&r, 0x40000001, 7, 78, 1500) && "an answer must carry the request id");
    assert(!xip_quic_reply_find(&r, 0x40000001, 8, 77, 1500) && !xip_quic_reply_find(&r, 0x40000002, 7, 77, 1500));
    assert(!xip_quic_reply_find(&r, 0x40000001, 7, 77, 1000 + XIP_QUIC_REPLY_MS + 1));                          /* expired */
    xip_quic_reply_note(&r, 0x40000001, 7, 78, &rq, 1100);                                                     /* two requests of one peer at once */
    assert(xip_quic_reply_find(&r, 0x40000001, 7, 77, 1500) && xip_quic_reply_find(&r, 0x40000001, 7, 78, 1500));
    xip_quic_reply_done(&r, f);
    assert(!xip_quic_reply_find(&r, 0x40000001, 7, 77, 1500) && xip_quic_reply_find(&r, 0x40000001, 7, 78, 1500));
    for (unsigned i = 0; i < XIP_QUIC_REPLIES + 2; i++) xip_quic_reply_note(&r, 1, i, 0, &rq, 10);              /* full: oldest replaced, no overflow */
    assert(xip_quic_reply_find(&r, 1, XIP_QUIC_REPLIES + 1, 0, 10));
    /* an expired note makes room before a live one is dropped */
    xip_quic_replies q = {0};
    for (unsigned i = 0; i < XIP_QUIC_REPLIES; i++) xip_quic_reply_note(&q, 1, i, 0, &rq, i == 3 ? 0 : 9000);   /* note 3 is old by the time the table is full */
    xip_quic_reply_note(&q, 1, 100, 0, &rq, 9000);
    assert(xip_quic_reply_find(&q, 1, 100, 0, 9000) && !xip_quic_reply_find(&q, 1, 3, 0, 9000) && xip_quic_reply_find(&q, 1, 0, 0, 9000));
}

static void test_owned_weighted_relays(void)
{
    xip_quic_table t={0};xip_relay_route r={0},got={0};xip_quic_entry e={0};
    r.dcid[0]=42;r.dcid_len=1;r.hop_count=3;
    for(unsigned i=0;i<3;i++){
        r.hops[i].link=100+i;r.hops[i].token=200+i;r.hops[i].weight=i;
        r.hops[i].cid_len=1;r.hops[i].rewritten_dcid[0]=50+i;
    }
    assert(xip_quic_relay_set(&t,1,&r,1)==XIP_ERR_NOTFOUND);
    assert(xip_quic_relay_set(&t,1,&r,0)==0);
    assert(xip_quic_relay_set(&t,1,&r,0)==XIP_ERR_BUSY);
    assert(xip_quic_relay_set(&t,2,&r,1)==XIP_ERR_BUSY);
    assert(xip_quic_relay_get(&t,2,0,&got)==XIP_ERR_NOTFOUND);
    assert(xip_quic_relay_get(&t,1,0,&got)==0 && got.hop_count==3);
    unsigned counts[3]={0};
    for(unsigned i=0;i<300;i++){
        assert(xip_quic_relay_select(&t,r.dcid,1,&e)==0);
        assert(e.token==e.egress+100 && e.out_cid[0]==e.egress-50);
        counts[e.egress-100]++;
    }
    assert(counts[0]==0 && counts[1]==100 && counts[2]==200);
    r.hops[1].weight=r.hops[2].weight=0;
    assert(xip_quic_relay_set(&t,1,&r,1)==XIP_ERR_ARG);
    assert(xip_quic_relay_get(&t,1,0,&got)==0 && got.hops[2].weight==2);
    r.hops[0].weight=1;
    assert(xip_quic_relay_set(&t,1,&r,1)==0);
    assert(xip_quic_relay_select(&t,r.dcid,1,&e)==0 && e.egress==100);
    assert(xip_quic_relay_remove(&t,2,r.dcid,1)==XIP_ERR_BUSY);
    xip_quic_relay_release(&t,2);assert(t.n==1);
    xip_quic_relay_release(&t,1);assert(t.n==0);
    assert(xip_quic_relay_remove(&t,1,r.dcid,1)==XIP_ERR_NOTFOUND);
    xip_quic_table_clear(&t);
}

static void test_relay_allocation_activity_and_expiry(void)
{
    xip_quic_table t={0};xip_relay_route request={0},r,other;xip_relay_stats stats;xip_quic_entry e;
    request.hop_count=1;request.hops[0].link=10;request.hops[0].token=9;request.hops[0].weight=1;
    request.hops[0].cid_len=1;request.hops[0].rewritten_dcid[0]=7;
    r=request;assert(xip_quic_relay_create(&t,1,&r,100,true,1000)==0);assert(r.dcid_len==8);
    other=request;assert(xip_quic_relay_create(&t,1,&other,100,true,2000)==1 && !memcmp(r.dcid,other.dcid,8));
    assert(xip_quic_relay_select_at(&t,r.dcid,8,90000,&e)==0);
    assert(xip_quic_relay_stats(&t,1,r.dcid,8,&stats)==0 && stats.last_used_us==90000);
    assert(xip_quic_relay_expire(&t,1,180000)==0);
    assert(xip_quic_relay_select_at(&t,r.dcid,8,190000,&e)==XIP_ERR_NOTFOUND);
    assert(xip_quic_relay_expire(&t,1,190000)==1 && !t.n && !t.e);
    other=request;assert(xip_quic_relay_create(&t,1,&other,0,true,200000)==0 && memcmp(r.dcid,other.dcid,8));
    assert(xip_quic_relay_expire(&t,1,9999999)==0);
    assert(xip_quic_relay_idle(&t,1,other.dcid,8,1)==0);
    assert(xip_quic_relay_expire(&t,1,9999999)==1);
    xip_quic_table_clear(&t);
}

int main(void)
{
    test_classify();
    test_owned_weighted_relays();
    test_relay_allocation_activity_and_expiry();
    test_build_and_envelope();
    test_table();
    test_alloc();
    test_rewrite();
    test_replies();
    puts("test_quic ok");
    return 0;
}
