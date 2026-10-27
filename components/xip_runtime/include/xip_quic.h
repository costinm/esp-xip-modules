/* SPDX-License-Identifier: Apache-2.0 */
/* The common QUIC receive function, pure part (components/xip_runtime/src/xip_quic.c): header classification, the table of
 * destination connection IDs (the "relay table"), the CBOR envelope fields and the bookkeeping of replies. No ESP-IDF, no locks, no
 * pool: it runs on a PC in tests/host/test_quic.c. The glue (links, pool packets, ownership, locking) is in xip_runtime.c.
 *
 * C does no QUIC termination: it reads the first byte, the version and the connection IDs, and decides who gets the packet. The
 * formats are those of quic-lite (/ws/dmesh/crates/quic-lite, read only): RFC 8999 invariants for long headers (first byte with the
 * form bit, 4-byte version, DCID length + DCID, SCID length + SCID), and for short headers a flags byte, the DCID and the packet
 * number. A short header has no length for the DCID: the receiver knows the lengths it issued. quic-lite writes a connection ID
 * with a class in the first byte (bit 7 clear: 1 byte; 100, 101, 110 in bits 7..5: 2, 4, 8 bytes; 111 reserved), which gives the
 * length without a table; a stack that issues other lengths sets one fixed length (`short_cid_len`). */
#pragma once

#include "xip.h"
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 1500 (the pool packet) - 48 (IPv6 40 + UDP 8, the largest prefix a forwarded packet takes) - 4 (suffix, to be confirmed). The
 * headroom in front of the packet stays empty so a packet can be forwarded as IPv6/UDP without a move. */
#define XIP_QUIC_MAX_PACKET 1448
#define XIP_QUIC_CID_MAX 20                      /* the longest connection ID (RFC 9000) kept in tables */
#define XIP_QUIC_TABLE_MAX 64                    /* entries of the relay table (about 16 relays are expected per device; it starts at 4 and doubles) */
#define XIP_QUIC_VERSION_DMESH 0x444d0001u       /* quic-lite: Initial / PathOpen / PathOpenAck long packets */
#define XIP_QUIC_VERSION_CBOR 0x444d0002u        /* "CBOR datagram": after the SCID comes the CBOR envelope {1: component, 2: method, 3: id, 5: fields} */

typedef enum {
    XIP_QUIC_NOT_QUIC = 0,       /* empty, or the fixed bit (0x40) is clear: not a QUIC packet; routing goes on */
    XIP_QUIC_MALFORMED,          /* looks like QUIC but a header is truncated or a length is invalid */
    XIP_QUIC_SHORT,
    XIP_QUIC_LONG,
} xip_quic_class;

/* What the first bytes of a packet say. The pointers point into the packet. For a long packet `body` is the offset after the SCID (what
 * the version defines); for a short one the offset after the DCID (the packet number, then the protected payload). */
typedef struct {
    uint8_t first;
    bool is_long;
    uint32_t version;            /* long only */
    const uint8_t *dcid; uint8_t dcid_len;
    const uint8_t *scid; uint8_t scid_len;           /* long only */
    uint16_t body;
} xip_quic_hdr;

/* Classify `p[0..len)`. `short_cid_len`: 0 = the quic-lite class rule, else the fixed DCID length of short headers. */
xip_quic_class xip_quic_classify(const uint8_t *p, size_t len, unsigned short_cid_len, xip_quic_hdr *h);

/* The length of a quic-lite connection ID from its first byte, or 0 for the reserved class. */
unsigned xip_quic_cid_class_len(uint8_t first);

/* Write a long header (first byte 0xc0, version, DCID, SCID) into `out`; returns its length, 0 when it does not fit or a CID is too long. */
size_t xip_quic_long_build(uint8_t *out, size_t cap, uint32_t version, const uint8_t *dcid, unsigned dcid_len, const uint8_t *scid, unsigned scid_len);

/* The fields of the CBOR envelope {1: component, 2: method, 3: id, 5: fields}. Component and method are required unsigned integers that fit
 * a positive int32; `id` is optional (0 = none: a notification; the packet number of the long header is not used, there is none in this
 * version). `response` is set when the map has a result (6) or an error (7): an answer, never to be dispatched as a request. Returns 0, or -1 when the map is malformed or the routing fields are missing. */
typedef struct { int32_t component, method; uint64_t id; bool has_id; bool response; } xip_quic_env;
int xip_quic_envelope(const uint8_t *cbor, size_t len, xip_quic_env *e);

/* ---- the relay table: destination connection IDs, a sorted vector that grows (up to XIP_QUIC_TABLE_MAX), issued and managed by the runtime ---- */
typedef enum {
    XIP_QUIC_DCID_LOCAL = 1,     /* terminated here: short packets go to the callback registered by the QUIC stack of fw/ */
    XIP_QUIC_DCID_RELAY = 2,     /* forwarded: to link `egress`, to the peer address `token`, with the DCID replaced by `out_cid` (as quic-lite's relay does) */
} xip_quic_kind;
typedef struct {
    uint8_t out_address[24], out_address_len;
    uint64_t last_used_us; uint32_t idle_ms;
    xip_relay_route *route; uint32_t owner, cursor;
    uint8_t cid[XIP_QUIC_CID_MAX]; uint8_t len, kind;
    uint32_t egress; uint64_t token;                       /* RELAY: the outgoing link and the peer's address token on it (MAC, IP) */
    uint8_t out_cid[XIP_QUIC_CID_MAX]; uint8_t out_len;    /* RELAY: the DCID the next hop knows this circuit by */
} xip_quic_entry;
typedef struct { xip_quic_entry *e; uint16_t n, cap; uint64_t next_relay_cid; } xip_quic_table;
/* Atomic owned route operations, shared with the native host tests. */
int xip_quic_relay_set(xip_quic_table *t, uint32_t owner, const xip_relay_route *route, unsigned mode);
int xip_quic_relay_get(const xip_quic_table *t, uint32_t owner, unsigned index, xip_relay_route *route);
int xip_quic_relay_remove(xip_quic_table *t, uint32_t owner, const uint8_t *cid, unsigned len);
void xip_quic_relay_release(xip_quic_table *t, uint32_t owner);
int xip_quic_relay_create(xip_quic_table *t, uint32_t owner, xip_relay_route *route, uint32_t idle_ms, bool reuse, uint64_t now_us);
int xip_quic_relay_stats(const xip_quic_table *t, uint32_t owner, const uint8_t *cid, unsigned len, xip_relay_stats *stats);
int xip_quic_relay_expire(xip_quic_table *t, uint32_t owner, uint64_t now_us);
int xip_quic_relay_idle(xip_quic_table *t, uint32_t owner, const uint8_t *cid, unsigned len, uint32_t idle_ms);
int xip_quic_relay_select_at(xip_quic_table *t, const uint8_t *cid, unsigned len, uint64_t now_us, xip_quic_entry *out);
/* Select one hop while holding the table lock. */
int xip_quic_relay_select(xip_quic_table *t, const uint8_t *cid, unsigned len, xip_quic_entry *out);
/* Insert or replace. 0, XIP_ERR_ARG (length 0 or above XIP_QUIC_CID_MAX, unknown kind, a RELAY without out_cid), XIP_ERR_NOMEM (full or out of memory). */
int xip_quic_table_add(xip_quic_table *t, const uint8_t *cid, unsigned len, unsigned kind, uint32_t egress, uint64_t token,
                       const uint8_t *out_cid, unsigned out_len);
int xip_quic_table_remove(xip_quic_table *t, const uint8_t *cid, unsigned len);       /* 0 or XIP_ERR_NOTFOUND */
const xip_quic_entry *xip_quic_table_find(const xip_quic_table *t, const uint8_t *cid, unsigned len);
void xip_quic_table_clear(xip_quic_table *t);                                          /* frees the storage */
/* The runtime issues the IDs. quic-lite's connection ID is a number of up to 61 bits encoded in 1 byte (values 1..127, bit 7 clear), 2 bytes (up to
 * 0x1fff: tag 100 in bits 7..5, 13 value bits), 4 bytes (tag 101, 29 bits) or 8 bytes (tag 110); value 0 is not used. `xip_quic_cid_encode` writes
 * the shortest form of `value` (returns its length, 0 for 0 or above 2^61-1). `xip_quic_table_alloc` takes the LOWEST free value (so freed IDs are
 * reused and 127 entries fit in 1-byte CIDs; two bytes would hold 8191, not 4k), adds the entry and returns its CID in `cid` (XIP_QUIC_CID_MAX bytes)
 * and `*len`; XIP_ERR_NOMEM when the table is full. */
unsigned xip_quic_cid_encode(uint64_t value, uint8_t *out);
int xip_quic_table_alloc(xip_quic_table *t, unsigned kind, uint32_t egress, uint64_t token, const uint8_t *out_cid, unsigned out_len, uint8_t *cid, unsigned *len);
/* Replace the DCID of a short packet at `p` (length `len`, classified as `h`) by `ncid`, as quic-lite's rewrite_dcid_in_place: flags, packet number and body
 * stay where they are; the new first byte and CID are written right before the packet number, so the packet starts `*shift` bytes later (negative:
 * earlier, into the `room` bytes of empty headroom in front of `p`). 0, or XIP_ERR_SIZE when it does not fit (nothing is changed). */
int xip_quic_short_rewrite(uint8_t *p, size_t len, const xip_quic_hdr *h, const uint8_t *ncid, unsigned nlen, size_t room, int *shift);

/* ---- replies: the answer to a CBOR datagram goes back as a CBOR datagram with the connection IDs swapped, to the peer address the request came from ----
 * A note is keyed by (link, source address token, request id); the runtime sends only an answer that carries that id (field 3) and consumes the note. */
#define XIP_QUIC_REPLIES 8
#define XIP_QUIC_REPLY_MS 5000
typedef struct {
    bool used; uint32_t link; uint64_t source; uint64_t id; uint32_t at_ms;
    uint8_t dcid[XIP_QUIC_CID_MAX], scid[XIP_QUIC_CID_MAX]; uint8_t dcid_len, scid_len;   /* of the reply: the request's SCID and DCID */
} xip_quic_reply;
typedef struct { xip_quic_reply r[XIP_QUIC_REPLIES]; uint8_t next; } xip_quic_replies;
void xip_quic_reply_note(xip_quic_replies *t, uint32_t link, uint64_t source, uint64_t id, const xip_quic_hdr *request, uint32_t now_ms);
/* The note for (link, source, id) that is not older than XIP_QUIC_REPLY_MS, or NULL. */
const xip_quic_reply *xip_quic_reply_find(const xip_quic_replies *t, uint32_t link, uint64_t source, uint64_t id, uint32_t now_ms);
void xip_quic_reply_done(xip_quic_replies *t, const xip_quic_reply *r);

/* ---- counters, one per way a packet can end ---- */
typedef struct {
    uint32_t rx;                 /* packets the function was shown */
    uint32_t declined;           /* not QUIC (fixed bit clear): routing went on */
    uint32_t malformed;          /* truncated or invalid header: dropped */
    uint32_t oversize;           /* above XIP_QUIC_MAX_PACKET: dropped */
    uint32_t short_local;        /* short packet to the local callback */
    uint32_t short_relay;        /* short packet forwarded by a relay entry */
    uint32_t short_relay_failed; /* relay entry whose egress link refused or is gone (no queue: dropped) */
    uint32_t short_unknown;      /* short packet, DCID not in the table: dropped */
    uint32_t short_no_handler;   /* local entry but no callback registered: dropped */
    uint32_t long_cbor;          /* 0x444d0002 packet routed to a component */
    uint32_t long_cbor_bad;      /* 0x444d0002 packet with a malformed envelope: dropped */
    uint32_t long_cbor_no_target;/* nobody serves the envelope's component, or its inbox is full: dropped */
    uint32_t long_cbor_response; /* 0x444d0002 packet that is an answer (field 6 or 7): given to the local callback, else dropped; never dispatched */
    uint32_t long_other;         /* any other long version, given to the local callback */
    uint32_t long_other_dropped; /* any other long version without a callback: dropped */
} xip_quic_stats;

#ifdef __cplusplus
}
#endif
