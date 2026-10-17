/* SPDX-License-Identifier: Apache-2.0 */
/* mod_test: calls every entry of the host table and reports one event per check. It is the device
 * test of the ABI (docs/testing.md) and the reference for how each call is meant to be used. Like every module it
 * never waits: what comes next is an alarm or an interrupt, and its progress lives in the state block.
 *
 * The start request picks the mode:
 *   (empty) or "selftest"  every check that needs no wiring; finishes with 0 when all pass
 *   "serve"                stay; requests are echoed as an event and answered with respond(); packets are echoed, kept, dropped
 *   "alarm"                one-shot, periodic and cancelled alarms
 *   "gpio A B"             pins A and B jumpered: levels, pull, and an interrupt on B when A toggles
 *   "spi SCK MISO MOSI CS [BUS]" a loopback wire from MOSI to MISO (BUS 2 or 3, default 2): spi_transfer returns what was sent
 * Events: 0x7e00 + n per check, text "<name> ok" or "<name> FAIL <code>"; 0x7eff the summary. */
/* mkmod: --state 64 --stack 4096 */
#include "xip_module.h"

#define EV_CHECK 0x7e00
#define EV_SUMMARY 0x7eff
#define EV_ECHO 0x7e10
#define EV_PKT 0x7e11

/* This module's own component and methods (the pair main or the module itself chose; see include/xip.h). */
#define C_TEST 2
#define M_ECHO 1      /* request {1: text}: echo as an event, answer {1: text} */
#define M_INFO 2      /* request: answer {1: calls, 2: mode} */
#define M_ONESHOT 10  /* alarm */
#define M_TICK 11     /* periodic alarm */
#define M_IRQ 12      /* interrupt of pin B */
#define M_MASKED 13   /* alarm: the masked interrupt stayed quiet */
#define M_TIMEOUT 14  /* alarm: the interrupt never came */
#define M_PKT_ECHO 20  /* a packet: flip its first byte and send it back to its sender, the same packet (no copy) */
#define M_PKT_REPLY 21 /* the answer to M_PKT_ECHO */
#define M_PKT_KEEP 22  /* a packet: keep it past the call */
#define M_PKT_DROP 23  /* give the kept packet back */
#define M_MEM_LEAK 24  /* take a block of memory and do not give it back (the host must notice at stop) */

typedef struct {
    uint8_t mode, done_oneshot, irq_seen;
    uint16_t n;
    int16_t pass, fail;
    int32_t a, b, periodic;
    uint32_t ticks, calls, kept;
    uint64_t t0;
} state_t;
_Static_assert(sizeof(state_t) <= 64, "state_t must fit the 64 bytes asked for with mkmod: --state");

enum { MODE_SELFTEST, MODE_SERVE, MODE_ALARM, MODE_GPIO };

static void check(state_t *st, const xip_host *h, const char *name, int ok, int code)
{
    char line[48];
    size_t n = 0, len = xip_strlen(name);
    if (len > 28) len = 28;
    xip_copy(line, name, len); n = len;
    if (ok) { xip_copy(line + n, " ok", 4); st->pass++; }
    else { xip_copy(line + n, " FAIL ", 6); n += 6; xip_itoa(line + n, code); st->fail++; xip_log(h, line); }
    xip_emit_text(h, (uint16_t)(EV_CHECK + st->n++), line);
}

/* The summary event; the result of the module: 0 when everything passed. */
static int finish(state_t *st, const xip_host *h)
{
    char sum[40]; size_t n = 0;
    xip_copy(sum, "pass=", 5); n = 5; n += xip_itoa(sum + n, st->pass);
    xip_copy(sum + n, " fail=", 6); n += 6; xip_itoa(sum + n, st->fail);
    xip_emit_text(h, EV_SUMMARY, sum);
    h->log(h->user, (const uint8_t *)sum, xip_strlen(sum));
    return st->fail ? -st->fail : XIP_RET_DONE;
}

static void cbor_checks(state_t *st, const xip_host *h)
{
    uint8_t buf[48];
    xip_cbor_w w;
    xip_cbor_init(&w, buf, sizeof(buf));
    xip_cbor_map(&w, 4);
    xip_cbor_uint(&w, 1); xip_cbor_int(&w, -68);
    xip_cbor_uint(&w, 2); xip_cbor_text(&w, "ok", 2);
    xip_cbor_uint(&w, 3); xip_cbor_bytes(&w, "\x01\x02\x03", 3);
    xip_cbor_uint(&w, 300); xip_cbor_uint(&w, 0x123456789aull);
    int n = xip_cbor_done(&w);
    check(st, h, "cbor-encode", n > 0 && buf[0] == 0xa4 && buf[1] == 0x01 && buf[2] == 0x38 && buf[3] == 67, n);
    int64_t v = 0; const uint8_t *s = 0; size_t sl = 0;
    check(st, h, "cbor-int", xip_cbor_get_int(buf, (size_t)n, 1, &v) == 0 && v == -68, (int)v);
    check(st, h, "cbor-text", xip_cbor_get_str(buf, (size_t)n, 2, XIP_CBOR_TEXT, &s, &sl) == 0 && sl == 2 && s[0] == 'o', 0);
    check(st, h, "cbor-bytes", xip_cbor_get_str(buf, (size_t)n, 3, XIP_CBOR_BYTES, &s, &sl) == 0 && sl == 3 && s[2] == 3, 0);
    check(st, h, "cbor-u64", xip_cbor_get_int(buf, (size_t)n, 300, &v) == 0 && v == 0x123456789aLL, 0);
    check(st, h, "cbor-absent", xip_cbor_get_int(buf, (size_t)n, 9, &v) == 1, 0);
    check(st, h, "cbor-wrong-type", xip_cbor_get_int(buf, (size_t)n, 2, &v) == -1, 0);
    check(st, h, "cbor-truncated", xip_cbor_get_int(buf, (size_t)n - 3, 300, &v) == -1, 0);
    xip_cbor_init(&w, buf, 4);
    xip_cbor_text(&w, "too long for four", 17);
    check(st, h, "cbor-overflow", xip_cbor_done(&w) == XIP_ERR_SIZE, 0);
}

/* Packets: a handle is valid until the packet leaves, a stale or zero handle is refused, `send` consumes the handle even when
 * it fails, a module holds only a few. */
static void pkt_checks(state_t *st, const xip_host *h)
{
    uint32_t headroom = 0, max = 0, p = 0;
    check(st, h, "pkt-info", h->pkt_info(h->user, &headroom, &max) == 0 && headroom >= XIP_PKT_HEADROOM && max >= 1, (int)max);
    int rc = h->pkt_alloc(h->user, &p);
    check(st, h, "pkt-alloc", rc == 0 && p != 0, rc);
    uint8_t *buf = 0; size_t room = 0;
    rc = h->pkt_buf(h->user, p, &buf, &room);
    check(st, h, "pkt-buf", rc == 0 && buf && room >= 1 && room <= XIP_PKT_MAX, rc);
    if (buf && room >= 8) { for (int i = 0; i < 8; i++) buf[i] = (uint8_t)(0xa0 + i); }
    check(st, h, "pkt-write", buf && room >= 8 && buf[7] == 0xa7, 0);
    check(st, h, "pkt-free", h->pkt_free(h->user, p) == 0, 0);
    check(st, h, "pkt-stale-free", h->pkt_free(h->user, p) == XIP_ERR_ARG, 0);
    check(st, h, "pkt-stale-buf", h->pkt_buf(h->user, p, &buf, &room) == XIP_ERR_ARG, 0);
    check(st, h, "pkt-zero", h->pkt_free(h->user, 0) == XIP_ERR_ARG && h->pkt_keep(h->user, 0) == XIP_ERR_ARG, 0);

    uint32_t q[16];
    int n = 0;
    while (n < 16 && h->pkt_alloc(h->user, &q[n]) == 0) n++;
    rc = n < 16 ? h->pkt_alloc(h->user, &p) : 0;
    check(st, h, "pkt-quota", n >= 1 && n < 16 && (rc == XIP_ERR_BUSY || rc == XIP_ERR_NOMEM), n);
    for (int i = 0; i < n; i++) h->pkt_free(h->user, q[i]);

    /* send gives the handle up whether it works or not */
    rc = h->pkt_alloc(h->user, &p);
    int rs = h->send(h->user, p, XIP_PKT_MAX + 1, XIP_ADDR_MAIN, C_TEST, M_PKT_REPLY, 1);
    check(st, h, "pkt-send-big", rc == 0 && rs == XIP_ERR_SIZE && h->pkt_free(h->user, p) == XIP_ERR_ARG, rs);
    rc = h->pkt_alloc(h->user, &p);
    rs = h->send(h->user, p, 4, 60000, C_TEST, M_PKT_REPLY, 1);
    check(st, h, "pkt-send-nowhere", rc == 0 && rs == XIP_ERR_NOTFOUND && h->pkt_free(h->user, p) == XIP_ERR_ARG, rs);
    rc = h->pkt_alloc(h->user, &p);
    rs = h->send(h->user, p, 4, XIP_ADDR_MAIN, C_TEST, M_PKT_REPLY, 2);        /* works or finds no hook: either way it is gone */
    check(st, h, "pkt-send-consumes", rc == 0 && (rs == 0 || rs == XIP_ERR_NOTFOUND) && h->pkt_free(h->user, p) == XIP_ERR_ARG, rs);
}

/* The managed heap: the host adds up what is taken and given back, the quota is a limit, the balance must be zero at stop. */
static void mem_checks(state_t *st, const xip_host *h)
{
    uint8_t *a = h->mem_alloc(h->user, 100), *b = h->mem_alloc(h->user, 100);
    check(st, h, "mem-alloc", a && b && a != b, 0);
    check(st, h, "mem-align", a && b && (((uintptr_t)a | (uintptr_t)b) & 7) == 0, 0);
    if (a) { for (int i = 0; i < 100; i++) a[i] = (uint8_t)i; }
    check(st, h, "mem-rw", a && a[99] == 99, 0);
    check(st, h, "mem-free", h->mem_free(h->user, a) == 0, 0);
    check(st, h, "mem-null", h->mem_free(h->user, 0) == XIP_ERR_ARG, 0);
    check(st, h, "mem-zero", h->mem_alloc(h->user, 0) == 0, 0);
    check(st, h, "mem-huge", h->mem_alloc(h->user, 1u << 24) == 0, 0);
    check(st, h, "mem-free-b", h->mem_free(h->user, b) == 0, 0);
}

static int selftest(state_t *st, const xip_host *h)
{
    check(st, h, "abi", h->abi_version == XIP_ABI_VERSION && h->size >= sizeof(*h), (int)h->abi_version);
    check(st, h, "log", h->log(h->user, (const uint8_t *)"mod_test log", 12) == XIP_OK, 0);
    uint64_t t0 = h->now_ms(h->user);
    check(st, h, "time", h->now_ms(h->user) >= t0, 0);

    if (h->features & XIP_FEAT_PACKETS) pkt_checks(st, h);
    if (h->features & XIP_FEAT_MEMORY) mem_checks(st, h);

    /* not a request: nothing to answer; connectivity is a plain query */
    check(st, h, "respond-no-request", h->respond(h->user, (const uint8_t *)"x", 1) == XIP_ERR_ARG, 0);
    int c = h->connected(h->user);
    check(st, h, "connected", c == 0 || c == 1, c);

    /* settings: own namespace, round trip, overwrite, erase, size error */
    static const char key[] = "t";
    uint8_t val[4] = {1, 2, 3, 4}, got[8]; size_t gl = 0;
    if (XIP_HOST_HAS(h, set_setting) && (h->features & XIP_FEAT_SETTINGS)) {
        check(st, h, "setting-set", h->set_setting(h->user, (const uint8_t *)key, 1, val, 4) == 0, 0);
        int rc = h->get_setting(h->user, (const uint8_t *)key, 1, got, sizeof(got), &gl);
        check(st, h, "setting-get", rc == 0 && gl == 4 && xip_equal(got, val, 4), rc);
        rc = h->get_setting(h->user, (const uint8_t *)key, 1, got, 2, &gl);
        check(st, h, "setting-small", rc == XIP_ERR_SIZE && gl == 4, rc);
        check(st, h, "setting-erase", h->set_setting(h->user, (const uint8_t *)key, 1, 0, 0) == 0, 0);
        rc = h->get_setting(h->user, (const uint8_t *)key, 1, got, sizeof(got), &gl);
        check(st, h, "setting-gone", rc == XIP_ERR_NOTFOUND, rc);
        check(st, h, "setting-key", h->get_setting(h->user, (const uint8_t *)"0123456789abcdefg", 17, got, 8, &gl) == XIP_ERR_ARG, 0);
    }

    /* services of the application: echo (tag 1) and uptime (tag 2) */
    if (h->features & XIP_FEAT_SERVICE) {
        uint8_t resp[16]; size_t rl = 0;
        int rc = h->call_service(h->user, 1, (const uint8_t *)"ping", 4, resp, sizeof(resp), &rl, 100);
        check(st, h, "service-echo", rc == 0 && rl == 4 && xip_equal(resp, "ping", 4), rc);
        rc = h->call_service(h->user, 2, 0, 0, resp, sizeof(resp), &rl, 100);
        check(st, h, "service-uptime", rc == 0 && rl == 8, rc);
        rc = h->call_service(h->user, 999, 0, 0, resp, sizeof(resp), &rl, 100);
        check(st, h, "service-missing", rc == XIP_ERR_NOTFOUND, rc);
    }

    /* alarms: argument validation, set and cancel (the firing is the "alarm" mode) */
    check(st, h, "alarm-bad-flags", h->alarm_set(h->user, 10, 0, 0, 0x80, C_TEST, M_ONESHOT) == XIP_ERR_ARG, 0);
    int al = h->alarm_set(h->user, 60000, 0, 0, XIP_ALARM_EXACT, C_TEST, M_ONESHOT);
    check(st, h, "alarm-set", al >= 0, al);
    check(st, h, "alarm-cancel", h->alarm_cancel(h->user, al) == 0 && h->alarm_cancel(h->user, al) == XIP_ERR_NOTFOUND, 0);

    /* argument validation: rejected before touching hardware */
    check(st, h, "gpio-bad-pin", h->gpio_config(h->user, 500, XIP_GPIO_OUTPUT, XIP_GPIO_PULL_NONE, 0) == XIP_ERR_ARG, 0);
    check(st, h, "gpio-unowned", h->gpio_read(h->user, 0) < 0, 0);
    check(st, h, "irq-bad-pin", h->irq_register(h->user, -1, XIP_IRQ_RISING, C_TEST, M_IRQ) < 0, 0);
    check(st, h, "irq-unknown", h->irq_unregister(h->user, 0) == XIP_ERR_NOTFOUND, 0);
    xip_spi_config bad = {.size = sizeof(bad), .bus = 2, .sck = -1, .miso = -1, .mosi = -1, .cs = -1, .mode = 9};
    check(st, h, "spi-bad-config", h->spi_open(h->user, &bad) < 0, 0);
    check(st, h, "spi-bad-handle", h->spi_transfer(h->user, 7, (const uint8_t *)"x", 0, 1) < 0 && h->spi_close(h->user, 7) < 0, 0);
    check(st, h, "i2c-bad-args", h->i2c_transfer(h->user, 0, 1, 2, 100000, 0x10, 0, 0, 0, 0, 10) < 0, 0);
    int raw = 0; uint32_t mv = 0;
    check(st, h, "adc-bad-pin", h->adc_read(h->user, 500, 3300, &raw, &mv, 0, 0) < 0, 0);

    /* emit: every value type, and a bad length/pointer pair */
    uint8_t one[8] = {1};
    check(st, h, "emit", h->emit(h->user, EV_ECHO, XIP_VALUE_BYTES, one, 8) == 0 &&
                         h->emit(h->user, EV_ECHO, XIP_VALUE_EMPTY, 0, 0) == 0, 0);
    check(st, h, "emit-bad", h->emit(h->user, EV_ECHO, XIP_VALUE_BYTES, 0, 4) == XIP_ERR_ARG, 0);

    cbor_checks(st, h);
    return finish(st, h);
}

static int parse_int(const uint8_t *s, size_t n, size_t *pos)
{
    while (*pos < n && s[*pos] == ' ') (*pos)++;
    int v = 0, any = 0;
    while (*pos < n && s[*pos] >= '0' && s[*pos] <= '9') { v = v * 10 + (s[*pos] - '0'); (*pos)++; any = 1; }
    return any ? v : -1;
}

/* "alarm": a 30 ms one-shot and a 10 ms periodic alarm that cancels itself after 3 ticks. */
static int alarm_start(state_t *st, const xip_host *h)
{
    st->t0 = h->now_ms(h->user);
    int one = h->alarm_set(h->user, 30, 0, 0, XIP_ALARM_EXACT, C_TEST, M_ONESHOT);
    st->periodic = h->alarm_set(h->user, 10, 0, 10, XIP_ALARM_EXACT, C_TEST, M_TICK);
    check(st, h, "alarms-set", one >= 0 && st->periodic >= 0 && one != st->periodic, one);
    return XIP_RET_CONTINUE;
}

static int alarm_fired(state_t *st, const xip_host *h, const xip_call *c)
{
    uint64_t dt = h->now_ms(h->user) - st->t0;
    if (c->method == M_ONESHOT) {
        st->done_oneshot = 1;
        check(st, h, "oneshot", dt >= 25 && dt < 500 && c->value >= 0, (int)dt);
    } else {
        st->ticks++;
        if (st->ticks == 3) {
            check(st, h, "periodic", dt >= 25 && dt < 500, (int)dt);
            check(st, h, "periodic-cancel", h->alarm_cancel(h->user, st->periodic) == 0, 0);
        }
    }
    return st->done_oneshot && st->ticks >= 3 ? finish(st, h) : XIP_RET_CONTINUE;
}

/* "gpio A B": A drives, B reads; B's edges arrive as M_IRQ calls. */
static int gpio_start(state_t *st, const xip_host *h, const uint8_t *req, size_t len)
{
    size_t pos = 4;
    st->a = parse_int(req, len, &pos); st->b = parse_int(req, len, &pos);
    if (st->a < 0 || st->b < 0) return -1;
    check(st, h, "cfg-a", h->gpio_config(h->user, st->a, XIP_GPIO_OUTPUT, XIP_GPIO_PULL_NONE, 0) == 0, 0);
    check(st, h, "cfg-b", h->gpio_config(h->user, st->b, XIP_GPIO_INPUT, XIP_GPIO_PULL_DOWN, 0) == 0, 0);
    h->gpio_write(h->user, st->a, 1);
    check(st, h, "high", h->gpio_read(h->user, st->b) == 1, 0);
    h->gpio_write(h->user, st->a, 0);
    check(st, h, "low", h->gpio_read(h->user, st->b) == 0, 0);
    check(st, h, "irq-register", h->irq_register(h->user, st->b, XIP_IRQ_BOTH, C_TEST, M_IRQ) == 0, 0);
    h->alarm_set(h->user, 150, 0, 0, XIP_ALARM_EXACT, C_TEST, M_TIMEOUT);
    h->gpio_write(h->user, st->a, 1);   /* the edge comes back as an M_IRQ call */
    return XIP_RET_CONTINUE;
}

static int gpio_irq(state_t *st, const xip_host *h, const xip_call *c)
{
    if (!st->irq_seen) {
        st->irq_seen = 1;
        check(st, h, "irq-event", c->value == 1, c->value);
        check(st, h, "irq-disable", h->irq_enable(h->user, st->b, 0) == 0, 0);
        h->gpio_write(h->user, st->a, 0);   /* masked: must stay quiet */
        h->alarm_set(h->user, 50, 0, 0, XIP_ALARM_EXACT, C_TEST, M_MASKED);
    } else {
        st->irq_seen++;
    }
    return XIP_RET_CONTINUE;
}

static int gpio_masked(state_t *st, const xip_host *h)
{
    check(st, h, "irq-masked", st->irq_seen == 1, st->irq_seen);
    check(st, h, "irq-unregister", h->irq_unregister(h->user, st->b) == 0, 0);
    check(st, h, "other-owner", h->gpio_write(h->user, st->a, 1) == 0, 0);
    return finish(st, h);
}

/* MOSI wired to MISO: the bytes come back. */
static int spi_loop(state_t *st, const xip_host *h, const uint8_t *req, size_t len)
{
    size_t pos = 3;
    int sck = parse_int(req, len, &pos), miso = parse_int(req, len, &pos), mosi = parse_int(req, len, &pos),
        cs = parse_int(req, len, &pos), bus = parse_int(req, len, &pos);
    if (sck < 0 || miso < 0 || mosi < 0 || cs < 0) return -1;
    if (bus >= 10) {   /* a debugging aid: another controller is open first (pins 32, 33, 25), then BUS - 10 */
        xip_spi_config d = {.size = sizeof(d), .bus = bus == 12 ? 3 : 2, .sck = 32, .miso = 33, .mosi = 33, .cs = 25,
                               .hz = 1000000, .mode = 0, .ready_pin = -1};
        h->spi_open(h->user, &d);
        bus -= 10;
    }
    xip_spi_config c = {.size = sizeof(c), .bus = bus < 0 ? 2 : bus, .sck = sck, .miso = miso, .mosi = mosi, .cs = cs,
                           .hz = 1000000, .mode = 0, .ready_pin = -1};
    int s = h->spi_open(h->user, &c);
    check(st, h, "open", s >= 0, s);
    if (s >= 0) {
        uint8_t tx[16], rx[16];
        for (int i = 0; i < 16; i++) { tx[i] = (uint8_t)(0xa0 + i); rx[i] = 0; }
        int rc = h->spi_transfer(h->user, s, tx, rx, 16);
        check(st, h, "transfer", rc == 0, rc);
        check(st, h, "loopback", xip_equal(tx, rx, 16), rx[0] << 8 | rx[1]);   /* on failure: the first two bytes that came back */
        check(st, h, "too-long", h->spi_transfer(h->user, s, tx, rx, 100000) == XIP_ERR_ARG, 0);
        check(st, h, "close", h->spi_close(h->user, s) == 0, 0);
    }
    return finish(st, h);
}

static int request(state_t *st, const xip_host *h, const xip_call *c)
{
    if (c->method == M_ECHO) {
        const uint8_t *text; size_t n;
        if (xip_cbor_get_str(c->data, c->len, 1, XIP_CBOR_TEXT, &text, &n) != 0) { text = c->data; n = 0; }
        h->emit(h->user, EV_ECHO, XIP_VALUE_TEXT, text, n);
        if (c->flags & 1) {
            uint8_t buf[96];
            xip_cbor_w w;
            xip_cbor_init(&w, buf, sizeof(buf));
            xip_cbor_map(&w, 1);
            xip_cbor_uint(&w, 1); xip_cbor_text(&w, (const char *)text, n);
            int len = xip_cbor_done(&w);
            if (len > 0) h->respond(h->user, buf, (size_t)len);
        }
    } else if (c->method == M_INFO && (c->flags & 1)) {
        uint8_t buf[16];
        xip_cbor_w w;
        xip_cbor_init(&w, buf, sizeof(buf));
        xip_cbor_map(&w, 2);
        xip_cbor_uint(&w, 1); xip_cbor_uint(&w, st->calls);
        xip_cbor_uint(&w, 2); xip_cbor_uint(&w, st->mode);
        int n = xip_cbor_done(&w);
        if (n > 0) h->respond(h->user, buf, (size_t)n);
    }
    return XIP_RET_CONTINUE;
}

XIP_HANDLER(test)(const xip_host *h, void *state, const xip_call *c)
{
    state_t *st = state;
    st->calls++;
    if (c->component == XIP_HOST_COMPONENT) {
        if (c->method == XIP_HOST_STOP) { xip_emit_text(h, EV_SUMMARY, "stopped"); return 0; }
        if (c->method != XIP_HOST_START) return XIP_RET_CONTINUE;   /* NETWORK, WAKE: not used */
        if (xip_starts(c->data, c->len, "serve")) { st->mode = MODE_SERVE; xip_log(h, "mod_test serving"); return XIP_RET_CONTINUE; }
        if (xip_starts(c->data, c->len, "alarm")) { st->mode = MODE_ALARM; return alarm_start(st, h); }
        if (xip_starts(c->data, c->len, "gpio ")) { st->mode = MODE_GPIO; return gpio_start(st, h, c->data, c->len); }
        if (xip_starts(c->data, c->len, "spi ")) return spi_loop(st, h, c->data, c->len);
        return selftest(st, h);
    }
    if (c->component != C_TEST) return XIP_RET_CONTINUE;
    switch (c->method) {
    case M_ONESHOT: case M_TICK: return alarm_fired(st, h, c);
    case M_IRQ: return gpio_irq(st, h, c);
    case M_MASKED: return gpio_masked(st, h);
    case M_TIMEOUT:
        if (st->irq_seen) return XIP_RET_CONTINUE;
        check(st, h, "irq-event", 0, -1);
        return finish(st, h);
    case M_PKT_ECHO:
        if (c->pkt) {
            uint8_t *b = 0; size_t room = 0;
            if (h->pkt_buf(h->user, c->pkt, &b, &room) == 0 && c->len) b[0] = (uint8_t)~b[0];
            (void)h->send(h->user, c->pkt, c->len, c->from, C_TEST, M_PKT_REPLY, c->id);
        }
        return XIP_RET_CONTINUE;
    case M_PKT_REPLY: xip_emit_text(h, EV_PKT, "reply"); return XIP_RET_CONTINUE;
    case M_PKT_KEEP:
        if (c->pkt && !st->kept && h->pkt_keep(h->user, c->pkt) == 0) st->kept = c->pkt;
        return XIP_RET_CONTINUE;
    case M_MEM_LEAK: (void)h->mem_alloc(h->user, 64); return XIP_RET_CONTINUE;
    case M_PKT_DROP:
        if (st->kept) { h->pkt_free(h->user, st->kept); st->kept = 0; }
        return XIP_RET_CONTINUE;
    default: return request(st, h, c);
    }
}
