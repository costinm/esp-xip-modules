/* SPDX-License-Identifier: Apache-2.0 */
/* The PPP framing of the runtime's serial line (tests/host/test_ppp.c). A record is opened by 0x7e, its first byte is the
 * port of xip.h, the rest is the payload with 0x7e and 0x7d escaped as 0x7d
 * (byte ^ 0x20), and the record closes with 0x7e. Records that no module holds
 * are read and stepped past; a record too long to fit is dropped whole and
 * counted. The wire speaks this framing exclusively: bytes between records are
 * not workload and are dropped — the person's console is the console module's
 * TCP lane, and the runtime's own log goes out raw. */
#include "xip_runtime.h"

#define PPP_FLAG 0x7e
#define PPP_ESC 0x7d
#define PPP_ESC_XOR 0x20

/* The states of the record decoder: WAIT - bytes between records are not
 * workload and are dropped; START - the port byte at the head, taken or the
 * record skipped; BODY - the record's bytes going into its receiver packet.
 * DISCARD merges into WAIT: over-long and malformed records and records of
 * ports nobody holds all skip to the next flag. */
static struct {
    enum { PPP_WAIT, PPP_START, PPP_BODY } state;
    bool escaped;
    size_t len;
    /* the receiver packet the record is being written into; one record - one packet */
    uint8_t *buf_data;
    size_t buf_cap;
    int slot;
    uint32_t port;
    bool pkt_open;
    bool control;
    uint8_t control_data[5];
    uint32_t dropped;
} ppp;

/* ---- the receiver: one pool packet, taken at the record's port ---- */

static void pkt_abort(void)
{
    if (!ppp.pkt_open) return;
    const xip_link_rx_buf b = {.slot = ppp.slot};
    xip_serial_frame_finish(ppp.port, &b, 0, XIP_ERR_IO);
    ppp.pkt_open = false;
}

static bool pkt_begin(uint32_t port)
{
    pkt_abort();
    xip_link_rx_buf b;
    const int rc = xip_serial_frame_begin(port, &b);
    ppp.pkt_open = rc == 0;
    if (rc < 0) ppp.dropped++;                  /* no packet in the pool: the record is lost, counted (1: nobody holds the port) */
    if (ppp.pkt_open) {
        ppp.buf_data = b.data;
        ppp.buf_cap = b.capacity;
        ppp.slot = b.slot;
        ppp.port = port;
    } else {
        ppp.buf_data = NULL;
        ppp.buf_cap = 0;
        ppp.slot = -1;
    }
    return ppp.pkt_open;
}

static void pkt_commit(size_t len)
{
    if (!ppp.pkt_open) return;
    const xip_link_rx_buf b = {.slot = ppp.slot};
    xip_serial_frame_finish(ppp.port, &b, len, 0);
    ppp.pkt_open = false;
}

/* A record byte: the byte, or -1 when it was a lone escape. */
static int unescape(uint8_t byte)
{
    if (ppp.escaped) {
        ppp.escaped = false;
        return (uint8_t)(byte ^ PPP_ESC_XOR);
    }
    if (byte == PPP_ESC) {
        ppp.escaped = true;
        return -1;
    }
    return byte;
}

static void push(const uint8_t *buf, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        const uint8_t byte = buf[i];
        if (byte == PPP_FLAG) {
            switch (ppp.state) {
            case PPP_WAIT:
                ppp.state = PPP_START;           /* the record's opening flag */
                break;
            case PPP_START:
                break;                          /* a doubled opening flag */
            case PPP_BODY:
                if (ppp.escaped || (!ppp.control && ppp.len == 0)) {
                    pkt_abort();
                    ppp.dropped++;
                } else {
                    if (ppp.control) {
                        if (xip_serial_control(ppp.port, ppp.control_data, ppp.len)) ppp.dropped++;
                    } else pkt_commit(ppp.len);
                }
                ppp.state = PPP_START; /* closing flag also opens the next record */
                break;
            }
            ppp.escaped = false;
            ppp.len = 0;
            continue;
        }
        switch (ppp.state) {
        case PPP_WAIT:
            break;                              /* not workload: dropped */
        case PPP_START: {
            const int b = unescape(byte);       /* the port byte */
            if (b < 0) break;
            ppp.control = b == 0xf8 || b == 0xf9;
            ppp.port = (uint32_t)b;
            if (ppp.control || (b != 0 && pkt_begin((uint32_t)b))) ppp.state = PPP_BODY;
            else ppp.state = PPP_WAIT;          /* port 0, or nobody holds it: the record is skipped */
            ppp.len = 0;
            break;
        }
        case PPP_BODY: {
            const int b = unescape(byte);
            if (b < 0) break;
            if (ppp.control && ppp.len < sizeof(ppp.control_data)) ppp.control_data[ppp.len++] = (uint8_t)b;
            else if (ppp.pkt_open && ppp.len < ppp.buf_cap) ppp.buf_data[ppp.len++] = (uint8_t)b;
            else {
                pkt_abort();
                ppp.dropped++;
                ppp.state = PPP_WAIT;           /* the rest of the record is not workload */
            }
            break;
        }
        }
    }
}

/* ---- the side that speaks : one framed record per port ---- */

#define PPP_TX_CHUNK 64
static int write_chunk(const uint8_t *data, size_t len)
{
    for (size_t at = 0; at < len;) {
        int written = xip_serial_write(data + at, len - at);
        if (written <= 0 || (size_t)written > len - at) return XIP_ERR_IO;
        at += (size_t)written;
    }
    return XIP_OK;
}
static int encode_tx(uint32_t port, const uint8_t *data, size_t len)
{
    uint8_t chunk[PPP_TX_CHUNK];
    size_t n = 0;
#define PUT(b) do { \
    chunk[n++] = (b); \
    if (n == sizeof(chunk)) { \
        if (write_chunk(chunk, n)) return XIP_ERR_IO; \
        n = 0; \
    } \
    } while (0)
#define PUT_ESCAPED(b) do { \
    const uint8_t c = (uint8_t)(b); \
    if (c == PPP_FLAG || c == PPP_ESC) { PUT(PPP_ESC); PUT(c ^ PPP_ESC_XOR); } \
    else PUT(c); \
} while (0)
    PUT(PPP_FLAG);
    PUT_ESCAPED(port);
    for (size_t i = 0; i < len; i++) PUT_ESCAPED(data[i]);
    PUT(PPP_FLAG);
    if (n && write_chunk(chunk, n)) return XIP_ERR_IO;
    return 0;
}

static void on_bytes(const uint8_t *buf, size_t n) { push(buf, n); }

static void on_reset(void)
{
    /* the input was flushed or lost bytes: forget the partial record */
    if (ppp.state == PPP_BODY) pkt_abort();
    ppp.state = PPP_WAIT;
    ppp.escaped = false;
    ppp.len = 0;
}

static int tx(uint32_t port, const uint8_t *data, size_t len) { return encode_tx(port, data, len); }

const xip_serial_codec_t xip_ppp_codec = {
    .on_bytes = on_bytes,
    .on_reset = on_reset,
    .tx = tx,
};

uint32_t xip_ppp_dropped(void) { return ppp.dropped; }
