/* SPDX-License-Identifier: Apache-2.0 */
/* The ESP-NOW-compatible action frame: DMesh's dmesh_rawnan::espnow codec, in C, with no allocation. The Rust twin is
 * crates/xip-runtime-esp/src/espnow_frame.rs; both are checked against tests/data/espnow-frames.txt, generated from DMesh's code
 * (tests/host/test_espnow_frame.c). A datagram of up to 1400 bytes travels as a vendor action (category 127, Espressif OUI
 * 18:fe:34): only the first bytes are in the ESP format (the 8-byte action header and one vendor element), the rest follows raw (DMesh's
 * FirstIeThenTail). The parser also reads DMesh's older sequence of 250-byte elements. */
#pragma once
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#define XIP_NOW_IEEE80211_HEADER 24
#define XIP_NOW_ACTION_HEADER 8
#define XIP_NOW_IE_HEADER 7
#define XIP_NOW_MAX_PAYLOAD 1400
#define XIP_NOW_MAX_IE_BODY 250
#define XIP_NOW_VENDOR_IE_ID 0xdd
#define XIP_NOW_ACTION_TYPE 0x04
#define XIP_NOW_V2 2

static const uint8_t xip_now_prefix[4] = {0x7f, 0x18, 0xfe, 0x34};

/* Bytes of a frame carrying `payload_len` bytes. */
static inline size_t xip_now_frame_len(size_t payload_len)
{
    return XIP_NOW_IEEE80211_HEADER + XIP_NOW_ACTION_HEADER + (payload_len ? XIP_NOW_IE_HEADER : 0) + payload_len;
}

/* Encode one vendor action frame into `out`: the length, or 0 when the payload is too large or `cap` too small. */
static inline size_t xip_now_encode(uint8_t *out, size_t cap, const uint8_t dest[6], const uint8_t src[6], const uint8_t bssid[6],
                                    const uint8_t *payload, size_t len)
{
    if (len > XIP_NOW_MAX_PAYLOAD) return 0;
    const size_t used = xip_now_frame_len(len);
    if (cap < used) return 0;
    memset(out, 0, used);
    out[0] = 0xd0;
    memcpy(out + 4, dest, 6);
    memcpy(out + 10, src, 6);
    memcpy(out + 16, bssid, 6);
    uint8_t *action = out + XIP_NOW_IEEE80211_HEADER;
    memcpy(action, xip_now_prefix, 4);
    memset(action + 4, 0xff, 4);                       /* the random value of the envelope; parsers attach no meaning to it */
    if (len) {
        /* one valid vendor element carries the first bytes of the datagram, the rest follows raw */
        const size_t first = len < XIP_NOW_MAX_IE_BODY ? len : XIP_NOW_MAX_IE_BODY;
        uint8_t *ie = action + XIP_NOW_ACTION_HEADER;
        ie[0] = XIP_NOW_VENDOR_IE_ID;
        ie[1] = (uint8_t)(5 + first);
        memcpy(ie + 2, xip_now_prefix + 1, 3);
        ie[5] = XIP_NOW_ACTION_TYPE;
        ie[6] = XIP_NOW_V2;
        memcpy(ie + XIP_NOW_IE_HEADER, payload, len);
    }
    return used;
}

/* Parse the action body a native receive callback supplies into `out`: the datagram length, or -1. */
static inline long xip_now_parse_body(const uint8_t *action, size_t action_len, uint8_t *out, size_t cap)
{
    if (action_len < 4 || memcmp(action, xip_now_prefix, 4)) return -1;
    size_t at = XIP_NOW_ACTION_HEADER, written = 0;
    for (;;) {
        if (at + 2 > action_len) return -1;
        const size_t element_len = action[at + 1];
        const size_t end = 2 + element_len;
        if (at + end > action_len) return -1;
        const uint8_t *element = action + at;
        if (element[0] != XIP_NOW_VENDOR_IE_ID || element_len < 5 || memcmp(element + 2, xip_now_prefix + 1, 3) ||
            element[5] != XIP_NOW_ACTION_TYPE || (element[6] & 0x0f) != XIP_NOW_V2) return -1;
        const size_t body = end - XIP_NOW_IE_HEADER;
        if (written + body > cap) return -1;
        memcpy(out + written, element + XIP_NOW_IE_HEADER, body);
        written += body;
        const int more = element[6] & 0x10;
        at += end;
        if (!more) {
            /* the one-element-plus-tail form is an RX-filter experiment: still one frame. Promiscuous metadata may keep the
             * 802.11 FCS: a complete v2 sequence has no tail but those four bytes, which are not datagram bytes. */
            const size_t tail = at <= action_len ? action_len - at : 0;
            if (tail == 4) return (long)written;
            if (written + tail > cap) return -1;
            memcpy(out + written, action + at, tail);
            return (long)(written + tail);
        }
    }
}

/* Parse a complete frame: the source address in `src`, the datagram length, or -1. */
static inline long xip_now_parse_frame(const uint8_t *frame, size_t len, uint8_t src[6], uint8_t *out, size_t cap)
{
    if (len < XIP_NOW_IEEE80211_HEADER + XIP_NOW_ACTION_HEADER + XIP_NOW_IE_HEADER || frame[0] != 0xd0) return -1;
    const long n = xip_now_parse_body(frame + XIP_NOW_IEEE80211_HEADER, len - XIP_NOW_IEEE80211_HEADER, out, cap);
    if (n >= 0) memcpy(src, frame + 10, 6);
    return n;
}
