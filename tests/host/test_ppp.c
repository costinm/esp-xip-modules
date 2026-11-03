/* The PPP codec of the serial line (components/xip_runtime/src/xip_ppp.c) against a fake packet pool: every record goes
 * into one packet taken at its port, and every packet taken is committed or aborted. */
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "xip_runtime.h"

extern const xip_serial_codec_t xip_ppp_codec;
extern uint32_t xip_ppp_dropped(void);

#define ROOM 32
static uint8_t room[ROOM];
static int open_packets, begun;
static uint32_t listening[4];
static struct { uint32_t port; uint8_t data[ROOM]; size_t len; } got[16];
static int ngot;
static uint8_t wire[512];
static size_t wire_len;

int xip_serial_frame_begin(uint32_t port, xip_link_rx_buf *buf)
{
    bool bound = false;
    for (int i = 0; i < 4; i++) bound |= listening[i] == port;
    if (!bound) return 1;
    assert(open_packets == 0);              /* one record, one packet */
    open_packets++;
    begun++;
    buf->data = room;
    buf->capacity = ROOM;
    buf->slot = 7;
    return 0;
}
void xip_serial_frame_finish(uint32_t port, const xip_link_rx_buf *buf, size_t len, int outcome)
{
    (void)buf;
    assert(open_packets == 1);
    open_packets--;
    if (outcome) { assert(outcome == XIP_ERR_IO); return; }
    got[ngot].port = port;
    got[ngot].len = len;
    memcpy(got[ngot++].data, room, len);
}
static int controls;
int xip_serial_control(uint32_t port, const uint8_t *data, size_t len)
{
    assert(port == 0xf8 || port == 0xf9);
    assert(len == 1 && data[0] == 1);
    controls++;
    return 0;
}
int xip_serial_write(const void *data, size_t len)
{
    memcpy(wire + wire_len, data, len);
    wire_len += len;
    return (int)len;
}

static void reset(void)
{
    xip_ppp_codec.on_reset();
    ngot = 0;
    wire_len = 0;
}
static void frame(uint32_t port, const uint8_t *data, size_t len)
{
    size_t at = wire_len;
    assert(xip_ppp_codec.tx(port, data, len) == 0);
    (void)at;
}
static void feed_all(const uint8_t *b, size_t n, size_t piece)
{
    for (size_t i = 0; i < n; i += piece) xip_ppp_codec.on_bytes(b + i, n - i < piece ? n - i : piece);
}

int main(void)
{
    listening[0] = 2; listening[1] = 3; listening[2] = 0xf7;

    /* a record carries its port and escapes the flag and the escape */
    reset();
    const uint8_t payload[] = {1, 0x7e, 0x7d, 9};
    frame(2, payload, sizeof(payload));
    assert(wire[0] == 0x7e && wire[wire_len - 1] == 0x7e);
    for (size_t i = 1; i + 1 < wire_len; i++) assert(wire[i] != 0x7e);
    feed_all(wire, wire_len, wire_len);
    assert(ngot == 1 && got[0].port == 2 && got[0].len == 4 && !memcmp(got[0].data, payload, 4));
    assert(open_packets == 0);

    /* records in a row, fed one byte at a time */
    reset();
    frame(2, (const uint8_t *)"one", 3);
    frame(3, (const uint8_t *)"two", 3);
    feed_all(wire, wire_len, 1);
    assert(ngot == 2 && got[0].port == 2 && got[1].port == 3 && !memcmp(got[1].data, "two", 3));

    /* bytes between records are not workload and are dropped, text included */
    reset();
    const uint8_t text[] = "status\r\n";
    xip_ppp_codec.on_bytes(text, sizeof(text) - 1);
    frame(2, (const uint8_t *)"x", 1);
    xip_ppp_codec.on_bytes(text, sizeof(text) - 1);
    feed_all(wire, wire_len, wire_len);
    assert(ngot == 1 && open_packets == 0);

    /* the record of a port nobody holds is skipped, and the next is not */
    reset();
    frame(9, (const uint8_t *)"lost", 4);
    frame(3, (const uint8_t *)"kept", 4);
    int before = begun;
    feed_all(wire, wire_len, wire_len);
    assert(ngot == 1 && got[0].port == 3 && begun == before + 1);

    /* too long and empty records are dropped, and the packet goes back */
    reset();
    uint8_t big[40];
    memset(big, 7, sizeof(big));
    uint32_t dropped = xip_ppp_dropped();
    frame(2, big, sizeof(big));
    frame(2, (const uint8_t *)"", 0);
    frame(2, (const uint8_t *)"ok", 2);
    feed_all(wire, wire_len, wire_len);
    assert(ngot == 1 && got[0].len == 2 && xip_ppp_dropped() == dropped + 2);
    assert(open_packets == 0);

    /* a flush in the middle of a record gives the packet back */
    reset();
    frame(2, (const uint8_t *)"partial", 7);
    feed_all(wire, wire_len - 3, wire_len);
    assert(open_packets == 1);
    xip_ppp_codec.on_reset();
    assert(open_packets == 0 && ngot == 0);

    /* Wake/control records bypass the packet pool and preserve the next frame. */
    reset();
    const uint8_t control[] = {1};
    before = begun;
    frame(0xf8, control, sizeof(control));
    frame(3, (const uint8_t *)"after", 5);
    feed_all(wire, wire_len, 1);
    assert(controls == 1 && begun == before + 1 && open_packets == 0);
    assert(ngot == 1 && got[0].port == 3 && got[0].len == 5);

    puts("ppp: ok");
    return 0;
}
