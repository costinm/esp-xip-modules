/* SPDX-License-Identifier: Apache-2.0 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "sim_host.h"
#include "espmod_cbor.h"
#include "espmod_pool.h"

#define PINS 49
#define MAX_EVENTS 128
#define MAX_LOG 64
#define INBOX 1024
#define QUEUE 64
#define ALARMS 8

typedef struct { uint16_t id; uint8_t type; uint8_t len; uint8_t data[64]; } rec_t;
typedef struct { int component, method, value; unsigned flags; int request; int pslot; uint16_t pstart, plen; uint32_t from; uint64_t id; uint8_t data[INBOX]; size_t len; } call_t;   /* request 3: a packet */
typedef struct {
    int used, waiting_net, periodic;
    uint64_t due, fire;           /* scheduled time, and when it will actually be delivered */
    uint32_t interval, window, flags;
    int component, method;
} alarm_t;

struct sim {
    espmod_host table;
    espmod_handle_fn handler;
    uint8_t *state;
    uint64_t now;
    int ended, result, connected;
    uint32_t calls, window_first, window_period;
    int owner[PINS], level[PINS], dir[PINS], wire[PINS];
    struct { int on; int edge; int component, method; int masked; } irq[PINS];
    call_t queue[QUEUE]; int qn;
    alarm_t alarms[ALARMS];
    uint8_t *response; size_t response_len; int in_request, responded;
    rec_t events[MAX_EVENTS]; int nevents;
    char logs[MAX_LOG][100]; int nlogs;
    struct { char key[16]; uint8_t val[256]; size_t len; int used; } settings[8];
    int spi_open[4], spi_transfers;
    espmod_held held;
    espmod_mem mem;
    unsigned mem_leaked;
    void *blocks[8];
    int component;                /* the address of this module for send() */
    unsigned leaked;
};

#define S ((sim_t *)user)

static int h_log(void *user, const uint8_t *d, size_t n)
{
    if (!d) return ESPMOD_ERR_ARG;
    if (S->nlogs < MAX_LOG) {
        size_t c = n < 99 ? n : 99;
        memcpy(S->logs[S->nlogs], d, c);
        S->logs[S->nlogs++][c] = 0;
    }
    return n < 97 ? 0 : ESPMOD_ERR_SIZE;
}

static uint64_t h_now(void *user) { return S->now; }

static int h_respond(void *user, const uint8_t *d, size_t n)
{
    if (!S->in_request) return ESPMOD_ERR_ARG;
    if (S->responded) return ESPMOD_ERR_BUSY;
    if ((n && !d) || n > 512) return ESPMOD_ERR_SIZE;
    if (!S->response) S->response = malloc(512);
    if (n) memcpy(S->response, d, n);
    S->response_len = n; S->responded = 1;
    return 0;
}

static int h_emit(void *user, uint16_t id, uint8_t type, const uint8_t *v, size_t len)
{
    if (len && !v) return ESPMOD_ERR_ARG;
    if (S->nevents < MAX_EVENTS) {
        rec_t *r = &S->events[S->nevents++];
        r->id = id; r->type = type; r->len = len > 63 ? 63 : len;
        if (r->len) memcpy(r->data, v, r->len);
        r->data[r->len] = 0;
    }
    return 0;
}

static int h_service(void *user, uint16_t tag, const uint8_t *req, size_t n, uint8_t *resp, size_t cap, size_t *rl,
                     uint32_t to)
{
    (void)user; (void)to;
    if (rl) *rl = 0;
    if (tag == 1) { if (n > cap) return ESPMOD_ERR_SIZE; memcpy(resp, req, n); *rl = n; return 0; }
    if (tag == 2) { uint64_t m = S->now; if (cap < 8) return ESPMOD_ERR_SIZE; memcpy(resp, &m, 8); *rl = 8; return 0; }
    return ESPMOD_ERR_NOTFOUND;
}

static int find_setting(sim_t *s, const uint8_t *k, size_t n, int create)
{
    for (int i = 0; i < 8; i++) if (s->settings[i].used && strlen(s->settings[i].key) == n && !memcmp(s->settings[i].key, k, n)) return i;
    if (create) for (int i = 0; i < 8; i++) if (!s->settings[i].used) { s->settings[i].used = 1; memcpy(s->settings[i].key, k, n); s->settings[i].key[n] = 0; return i; }
    return -1;
}

static int h_get_setting(void *user, const uint8_t *k, size_t kn, uint8_t *v, size_t cap, size_t *vl)
{
    if (!k || !kn || kn > 15 || !vl || (cap && !v)) return ESPMOD_ERR_ARG;
    int i = find_setting(S, k, kn, 0);
    if (i < 0) return ESPMOD_ERR_NOTFOUND;
    *vl = S->settings[i].len;
    if (cap < S->settings[i].len) return ESPMOD_ERR_SIZE;
    memcpy(v, S->settings[i].val, S->settings[i].len);
    return 0;
}

static int h_set_setting(void *user, const uint8_t *k, size_t kn, const uint8_t *v, size_t vn)
{
    if (!k || !kn || kn > 15 || (vn && !v) || vn > 256) return ESPMOD_ERR_ARG;
    int i = find_setting(S, k, kn, vn != 0);
    if (!vn) { if (i >= 0) S->settings[i].used = 0; return 0; }
    if (i < 0) return ESPMOD_ERR_NOMEM;
    memcpy(S->settings[i].val, v, vn);
    S->settings[i].len = vn;
    return 0;
}

static int claim(sim_t *s, int pin) { if (pin < 0 || pin >= PINS) return ESPMOD_ERR_ARG; s->owner[pin] = 1; return 0; }
static int owned(sim_t *s, int pin) { return pin >= 0 && pin < PINS && s->owner[pin]; }

static void enqueue(sim_t *s, int component, int method, int value)
{
    if (s->qn >= QUEUE) return;
    call_t *c = &s->queue[s->qn++];
    memset(c, 0, offsetof(call_t, data));
    c->component = component; c->method = method; c->value = value;
}

static void drive(sim_t *s, int pin, int level)
{
    if (s->level[pin] == level) return;
    s->level[pin] = level;
    if (s->irq[pin].on && !s->irq[pin].masked && (s->irq[pin].edge == ESPMOD_IRQ_BOTH ||
        (s->irq[pin].edge == ESPMOD_IRQ_RISING && level) || (s->irq[pin].edge == ESPMOD_IRQ_FALLING && !level)))
        enqueue(s, s->irq[pin].component, s->irq[pin].method, level);
}

static int h_gpio_config(void *user, int pin, int mode, int pull, int level)
{
    int rc = claim(S, pin);
    if (rc) return rc;
    S->dir[pin] = mode;
    if (mode != ESPMOD_GPIO_INPUT) { S->level[pin] = level != 0; if (S->wire[pin] >= 0) drive(S, S->wire[pin], level != 0); }
    else if (pull == ESPMOD_GPIO_PULL_DOWN) S->level[pin] = 0;
    return 0;
}

static int h_gpio_read(void *user, int pin) { return owned(S, pin) ? S->level[pin] : ESPMOD_ERR_ARG; }

static int h_gpio_write(void *user, int pin, int level)
{
    if (!owned(S, pin)) return ESPMOD_ERR_ARG;
    S->level[pin] = level != 0;
    if (S->wire[pin] >= 0) drive(S, S->wire[pin], level != 0);
    return 0;
}

static int h_irq_register(void *user, int pin, int edge, int component, int method)
{
    int rc = claim(S, pin);
    if (rc) return rc;
    if (S->irq[pin].on) return ESPMOD_ERR_BUSY;
    S->irq[pin].on = 1; S->irq[pin].edge = edge; S->irq[pin].component = component; S->irq[pin].method = method;
    S->irq[pin].masked = 0;
    return 0;
}
static int h_irq_unregister(void *user, int pin) { if (pin < 0 || pin >= PINS || !S->irq[pin].on) return ESPMOD_ERR_NOTFOUND; S->irq[pin].on = 0; return 0; }
static int h_irq_enable(void *user, int pin, int en) { if (pin < 0 || pin >= PINS || !S->irq[pin].on) return ESPMOD_ERR_NOTFOUND; S->irq[pin].masked = !en; return 0; }

/* ---- alarms: the same rules as components/espmod_host ---- */
static uint32_t window_ms(sim_t *s)
{
    if (!s->window_period) return 0;
    if (s->now < s->window_first) return (uint32_t)(s->window_first - s->now);
    uint64_t into = (s->now - s->window_first) % s->window_period;
    return (uint32_t)(s->window_period - into);
}

static uint32_t fire_delay(sim_t *s, uint32_t delay, uint32_t window, uint32_t flags)
{
    uint32_t w = window_ms(s);
    if (!(flags & ESPMOD_ALARM_FLEXIBLE) || !w) return delay;
    if (w >= delay && w - delay <= window) return w;
    return w < delay ? delay : delay + window;
}

static int h_alarm_set(void *user, uint32_t delay, uint32_t window, uint32_t interval, uint32_t flags, int component, int method)
{
    if (flags & ~(uint32_t)(ESPMOD_ALARM_FLEXIBLE | ESPMOD_ALARM_CONNECTED | ESPMOD_ALARM_WINDOW)) return ESPMOD_ERR_ARG;
    for (int i = 0; i < ALARMS; i++) {
        alarm_t *a = &S->alarms[i];
        if (a->used) continue;
        *a = (alarm_t){.used = 1, .interval = interval, .window = window, .flags = flags, .component = component,
                       .method = method, .due = S->now + delay, .fire = S->now + fire_delay(S, delay, window, flags)};
        return i;
    }
    return ESPMOD_ERR_NOMEM;
}

static int h_alarm_cancel(void *user, int i)
{
    if (i < 0 || i >= ALARMS || !S->alarms[i].used) return ESPMOD_ERR_NOTFOUND;
    S->alarms[i].used = 0;
    return 0;
}

static int h_connected(void *user) { return S->connected; }

static int h_spi_open(void *user, const espmod_spi_config *c)
{
    if (!c || c->size < sizeof(*c) || c->mode < 0 || c->mode > 3 || c->sck < 0 || c->cs < 0) return ESPMOD_ERR_ARG;
    for (int i = 0; i < 4; i++) if (!S->spi_open[i]) { S->spi_open[i] = 1; return i; }
    return ESPMOD_ERR_NOMEM;
}
static int h_spi_transfer(void *user, int h, const uint8_t *tx, uint8_t *rx, size_t len)
{
    if (h < 0 || h > 3 || !S->spi_open[h] || !tx || !len || len > 272) return ESPMOD_ERR_ARG;
    S->spi_transfers++;
    if (rx) memcpy(rx, tx, len);   /* MOSI wired to MISO */
    return 0;
}
static int h_spi_close(void *user, int h) { if (h < 0 || h > 3 || !S->spi_open[h]) return ESPMOD_ERR_ARG; S->spi_open[h] = 0; return 0; }
static int h_i2c(void *user, int port, int sda, int scl, uint32_t f, uint8_t a, const uint8_t *tx, size_t tn, uint8_t *rx, size_t rn, uint32_t to)
{
    (void)user; (void)port; (void)sda; (void)scl; (void)f; (void)a; (void)tx; (void)rx; (void)to;
    return (!tn && !rn) ? ESPMOD_ERR_ARG : ESPMOD_ERR_IO;
}
static int h_adc(void *user, int pin, uint32_t ref, int *raw, uint32_t *mv, int *unit, int *ch)
{
    (void)user; (void)ref; (void)unit; (void)ch;
    if (!raw || !mv || pin < 0 || pin >= PINS) return ESPMOD_ERR_ARG;
    return ESPMOD_ERR_UNSUPPORTED;
}
static int h_led(void *user, int pin, uint8_t r, uint8_t g, uint8_t b) { (void)r; (void)g; (void)b; return claim(S, pin); }


/* Wi-Fi requested by the module. */
char sim_ssid[40], sim_psk[72];
int sim_wifi_on;
static char sim_saved_ssid[40], sim_saved_psk[72];     /* what wifi_config stored */
static int h_wifi_config(void *user, const uint8_t *ssid, size_t sn, const uint8_t *psk, size_t pn)
{
    (void)user;
    if (sn > 32 || pn > 63) return ESPMOD_ERR_ARG;
    memcpy(sim_saved_ssid, ssid, sn); sim_saved_ssid[sn] = 0;
    memcpy(sim_saved_psk, psk, pn); sim_saved_psk[pn] = 0;
    return 0;
}
static int h_wifi_start(void *user, const uint8_t *ssid, size_t sn, const uint8_t *psk, size_t pn)
{
    (void)user;
    if (!sn) { snprintf(sim_ssid, sizeof(sim_ssid), "%s", sim_saved_ssid); snprintf(sim_psk, sizeof(sim_psk), "%s", sim_saved_psk); }   /* the stored network */
    else { memcpy(sim_ssid, ssid, sn); sim_ssid[sn] = 0; memcpy(sim_psk, psk, pn); sim_psk[pn] = 0; }
    sim_wifi_on = 1;
    return 0;
}
static int h_wifi_scan(void *user, uint8_t *out, size_t cap)
{
    (void)user;
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    em_cbor_array(&w, 2);
    static const uint8_t b1[6] = {0x10, 0xbd, 0xa3, 0xac, 0x5a, 0x21}, b2[6] = {1, 2, 3, 4, 5, 6};
    em_cbor_map(&w, 5);
    em_cbor_uint(&w, 1); em_cbor_bytes(&w, (const uint8_t *)"costin", 6);
    em_cbor_uint(&w, 2); em_cbor_bytes(&w, b1, 6);
    em_cbor_uint(&w, 3); em_cbor_int(&w, -41);
    em_cbor_uint(&w, 4); em_cbor_uint(&w, 6);
    em_cbor_uint(&w, 5); em_cbor_uint(&w, 3);
    em_cbor_map(&w, 5);
    em_cbor_uint(&w, 1); em_cbor_bytes(&w, (const uint8_t *)"", 0);
    em_cbor_uint(&w, 2); em_cbor_bytes(&w, b2, 6);
    em_cbor_uint(&w, 3); em_cbor_int(&w, -80);
    em_cbor_uint(&w, 4); em_cbor_uint(&w, 11);
    em_cbor_uint(&w, 5); em_cbor_uint(&w, 0);
    return em_cbor_done(&w);
}
static int h_heap_info(void *user, uint8_t *out, size_t cap)
{
    (void)user;
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    em_cbor_map(&w, 6);
    static const uint32_t v[6] = {41000, 30000, 28000, 230000, 38000, 26000};
    for (int i = 0; i < 6; i++) { em_cbor_uint(&w, (uint64_t)i + 1); em_cbor_uint(&w, v[i]); }
    return em_cbor_done(&w);
}
static int h_heap_dump(void *user, int kind) { (void)user; return kind >= 0 && kind <= 5 ? 0 : ESPMOD_ERR_ARG; }
static char sim_nv_val[16];
static int h_nv(void *user, const uint8_t *key, size_t kn, const uint8_t *value, size_t vn, uint8_t *out, size_t cap)
{
    (void)user;
    if (kn != 14 || memcmp(key, "wifi.rx_static", 14)) return ESPMOD_ERR_NOTFOUND;
    if (value) {
        if (vn >= sizeof(sim_nv_val)) return ESPMOD_ERR_ARG;
        memcpy(sim_nv_val, value, vn); sim_nv_val[vn] = 0;
        return 0;
    }
    size_t n = strlen(sim_nv_val);
    if (!n) return ESPMOD_ERR_NOTFOUND;
    if (n > cap) return ESPMOD_ERR_SIZE;
    memcpy(out, sim_nv_val, n);
    return (int)n;
}
static int h_wifi_stop(void *user) { (void)user; sim_wifi_on = 0; return 0; }
static int h_wifi_info(void *user, uint8_t *out, size_t cap)
{
    (void)user;
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    em_cbor_map(&w, (sim_wifi_on ? 4 : 1) + (sim_saved_ssid[0] ? 2 : 0));
    em_cbor_uint(&w, 1); em_cbor_uint(&w, sim_wifi_on ? 2 : 0);
    if (sim_wifi_on) {
        em_cbor_uint(&w, 2); em_cbor_text(&w, "lab", 3);
        em_cbor_uint(&w, 3); em_cbor_text(&w, "192.168.49.7", 12);
        em_cbor_uint(&w, 4); em_cbor_int(&w, -40);
    }
    if (sim_saved_ssid[0]) {
        em_cbor_uint(&w, 5); em_cbor_text(&w, sim_saved_ssid, strlen(sim_saved_ssid));
        em_cbor_uint(&w, 6); em_cbor_bool(&w, sim_saved_psk[0] != 0);
    }
    return em_cbor_done(&w);
}
/* The console's calls: the serial port, other modules, the module list, events, reset */
int sim_serial_component = -1, sim_serial_method = -1, sim_reset_called;
char sim_started[40], sim_start_request[80], sim_stopped[40];
uint8_t sim_modules_cbor[600], sim_events_cbor[600];
size_t sim_modules_len, sim_events_len;
char sim_serial_out[4096];
size_t sim_serial_len;
void sim_serial_clear(void) { sim_serial_len = 0; sim_serial_out[0] = 0; }
static int h_serial_open(void *user, int component, int method) { (void)user; sim_serial_component = component; sim_serial_method = method; return 0; }
static int h_serial_write(void *user, const uint8_t *d, size_t n)
{
    (void)user;
    for (size_t i = 0; i < n && sim_serial_len + 1 < sizeof(sim_serial_out); i++) sim_serial_out[sim_serial_len++] = (char)d[i];
    sim_serial_out[sim_serial_len] = 0;
    return (int)n;
}
static int h_serial_close(void *user) { (void)user; sim_serial_component = -1; return 0; }
static int h_device_info(void *user, uint8_t *out, size_t cap)
{
    (void)user;
    em_cbor_w w; em_cbor_init(&w, out, cap);
    em_cbor_map(&w, 5);
    em_cbor_uint(&w, 1); em_cbor_map(&w, 1); em_cbor_uint(&w, 4); em_cbor_text(&w, "main", 4);
    em_cbor_uint(&w, 2); em_cbor_map(&w, 0);
    em_cbor_uint(&w, 3); em_cbor_map(&w, 0);
    em_cbor_uint(&w, 4); em_cbor_put(&w, sim_modules_cbor, sim_modules_len);
    em_cbor_uint(&w, 5); em_cbor_map(&w, 0);
    return em_cbor_done(&w);
}
static int h_modules_info(void *user, uint8_t *out, size_t cap) { (void)user; if (sim_modules_len > cap) return ESPMOD_ERR_SIZE; memcpy(out, sim_modules_cbor, sim_modules_len); return (int)sim_modules_len; }
static int h_events_info(void *user, uint8_t *out, size_t cap) { (void)user; if (sim_events_len > cap) return ESPMOD_ERR_SIZE; memcpy(out, sim_events_cbor, sim_events_len); return (int)sim_events_len; }
static int h_module_start(void *user, const uint8_t *name, size_t n, const uint8_t *req, size_t rn)
{
    (void)user;
    if (n != 4 || memcmp(name, "test", 4)) return ESPMOD_ERR_NOTFOUND;
    memcpy(sim_started, name, n); sim_started[n] = 0;
    memcpy(sim_start_request, req, rn); sim_start_request[rn] = 0;
    return 0;
}
static int h_module_stop(void *user, const uint8_t *name, size_t n)
{
    (void)user;
    if (n != 4 || memcmp(name, "test", 4)) return ESPMOD_ERR_NOTFOUND;
    memcpy(sim_stopped, name, n); sim_stopped[n] = 0;
    return 0;
}
uint8_t sim_manifest[200];
size_t sim_manifest_len;
static int h_module_manifest(void *user, const uint8_t *name, size_t n, uint8_t *out, size_t cap)
{
    (void)user;
    if (n != 4 || memcmp(name, "test", 4)) return ESPMOD_ERR_NOTFOUND;
    if (sim_manifest_len > cap) return ESPMOD_ERR_SIZE;
    memcpy(out, sim_manifest, sim_manifest_len);
    return (int)sim_manifest_len;
}
static int h_reset(void *user) { (void)user; sim_reset_called = 1; return 0; }

/* ---- packets: the same contract as components/espmod_host, over the same pool code ---- */
#define SIM_SLOTS 8
#define SIM_QUOTA 4
#define SIM_INBOX 4
#define SIM_MEM_QUOTA 8192       /* explicit small cap for quota tests */
static espmod_pool *sim_pool;
static sim_t *routes[8];
sim_msg_t sim_main_msgs[8];
int sim_main_n;

static espmod_pool *pool(void)
{
    if (!sim_pool) sim_pool = espmod_pool_create(SIM_SLOTS, ESPMOD_PKT_SLOT_MIN);
    return sim_pool;
}
int sim_pool_available(void) { return espmod_pool_available(pool()); }
unsigned sim_held_count(sim_t *s) { return espmod_held_count(&s->held, ESPMOD_HELD_OWNED); }
unsigned sim_leaked(sim_t *s) { return s->leaked; }
void sim_main_clear(void) { sim_main_n = 0; }
void sim_set_component(sim_t *s, int component)
{
    s->component = component;
    for (int i = 0; i < 8; i++) if (!routes[i] || routes[i] == s) { routes[i] = s; return; }
}
static sim_t *route(int component)
{
    for (int i = 0; i < 8; i++) if (routes[i] && routes[i]->component == component && !routes[i]->ended) return routes[i];
    return NULL;
}

static int h_pkt_alloc(void *user, uint32_t *out)
{
    if (!out) return ESPMOD_ERR_ARG;
    *out = 0;
    if (espmod_held_count(&S->held, ESPMOD_HELD_OWNED) >= S->held.quota) return ESPMOD_ERR_BUSY;
    int slot = espmod_pool_acquire(pool(), 2);
    if (slot < 0) return ESPMOD_ERR_NOMEM;
    *out = espmod_held_add(&S->held, slot, ESPMOD_PKT_HEADROOM, 0, ESPMOD_HELD_OWNED);
    return ESPMOD_OK;
}
static int h_pkt_buf(void *user, uint32_t pkt, uint8_t **ptr, size_t *max)
{
    espmod_held_entry *e = espmod_held_get(&S->held, pkt);
    if (!e || !ptr || !max) return ESPMOD_ERR_ARG;
    if (espmod_pool_refs(pool(), e->slot) != 1) return ESPMOD_ERR_BUSY;
    *ptr = espmod_pool_slot(pool(), e->slot) + e->start;
    *max = ESPMOD_PKT_MAX;
    return ESPMOD_OK;
}
static int h_pkt_keep(void *user, uint32_t pkt) { int rc = espmod_held_keep(&S->held, pkt); return rc == 0 ? 0 : rc == -2 ? ESPMOD_ERR_BUSY : ESPMOD_ERR_ARG; }
static int h_pkt_free(void *user, uint32_t pkt)
{
    espmod_held_entry e = espmod_held_take(&S->held, pkt);
    if (e.slot < 0) return ESPMOD_ERR_ARG;
    espmod_pool_release(pool(), e.slot);
    return 0;
}
static int h_pkt_info(void *user, uint32_t *headroom, uint32_t *max) { (void)user; *headroom = ESPMOD_PKT_HEADROOM; *max = ESPMOD_PKT_MAX; return 0; }

static int deliver(sim_t *t, int slot, uint16_t start, uint16_t len, int component, int method, uint32_t from, uint64_t id)
{
    if (t->ended) return ESPMOD_ERR_NOTFOUND;
    int waiting = 0;
    for (int k = 0; k < t->qn; k++) waiting += t->queue[k].request == 3;
    if (waiting >= SIM_INBOX || t->qn >= QUEUE) return ESPMOD_ERR_BUSY;           /* the runtime-side queue is full: back off */
    call_t *c = &t->queue[t->qn++];
    memset(c, 0, offsetof(call_t, data));
    c->component = component; c->method = method; c->request = 3;
    c->pslot = slot; c->pstart = start; c->plen = len; c->from = from; c->id = id;
    return 0;
}

static int h_send(void *user, uint32_t pkt, size_t len, uint32_t dst, int component, int method, uint64_t id)
{
    espmod_held_entry e = espmod_held_take(&S->held, pkt);
    if (e.slot < 0) return ESPMOD_ERR_ARG;
    int rc;
    if (len > ESPMOD_PKT_MAX) {
        rc = ESPMOD_ERR_SIZE;
    } else if (dst == ESPMOD_ADDR_MAIN) {
        if (sim_main_n < 8) {
            sim_msg_t *m = &sim_main_msgs[sim_main_n++];
            m->from = S->component; m->component = component; m->method = method; m->id = id; m->len = len > 256 ? 256 : len;
            memcpy(m->data, espmod_pool_slot(pool(), e.slot) + e.start, m->len);
        }
        espmod_pool_release(pool(), e.slot);
        return 0;
    } else {
        sim_t *t = route((int)dst);
        rc = t ? deliver(t, e.slot, e.start, (uint16_t)len, component, method, (uint32_t)S->component, id) : ESPMOD_ERR_NOTFOUND;
        if (rc == 0) return 0;
    }
    espmod_pool_release(pool(), e.slot);
    return rc;
}

int sim_send_from(sim_t *s, uint32_t from, int component, int method, uint64_t id, const void *data, size_t len)
{
    if (len > ESPMOD_PKT_MAX) return ESPMOD_ERR_SIZE;
    int slot = espmod_pool_acquire(pool(), 0);
    if (slot < 0) return ESPMOD_ERR_NOMEM;
    if (len) memcpy(espmod_pool_slot(pool(), slot) + ESPMOD_PKT_HEADROOM, data, len);
    int rc = deliver(s, slot, ESPMOD_PKT_HEADROOM, (uint16_t)len, component, method, from, id);
    if (rc) espmod_pool_release(pool(), slot);
    return rc;
}

int sim_send(sim_t *s, int component, int method, uint64_t id, const void *data, size_t len)
{
    return sim_send_from(s, ESPMOD_ADDR_MAIN, component, method, id, data, len);
}

/* The balance is all the host tracks; the simulator also remembers the blocks so that sim_free can release a leaked one. */
static void *h_mem_alloc(void *user, size_t size)
{
    void *p = espmod_mem_alloc(&S->mem, size);
    for (int i = 0; p && i < 8; i++) if (!S->blocks[i]) { S->blocks[i] = p; break; }
    return p;
}
static int h_mem_free(void *user, void *ptr)
{
    for (int i = 0; i < 8; i++) if (S->blocks[i] == ptr) S->blocks[i] = NULL;
    return espmod_mem_free(&S->mem, ptr) == 0 ? 0 : ESPMOD_ERR_ARG;
}
unsigned sim_mem_leaked(sim_t *s) { return s->mem_leaked; }

/* The module used respond() on a packet that carried an id: the same bytes go to the sender as a message with that id. */
static void send_answer(sim_t *s, uint32_t to, int component, int method, uint64_t id, const uint8_t *data, size_t len)
{
    if (to == ESPMOD_ADDR_MAIN) {
        if (sim_main_n < 8) {
            sim_msg_t *m = &sim_main_msgs[sim_main_n++];
            m->from = s->component; m->component = component; m->method = method; m->id = id; m->len = len > 256 ? 256 : len;
            if (m->len) memcpy(m->data, data, m->len);
        }
        return;
    }
    sim_t *t = route((int)to);
    if (!t) return;
    int slot = espmod_pool_acquire(pool(), 0);
    if (slot < 0) return;
    if (len) memcpy(espmod_pool_slot(pool(), slot) + ESPMOD_PKT_HEADROOM, data, len);
    if (deliver(t, slot, ESPMOD_PKT_HEADROOM, (uint16_t)len, component, method, (uint32_t)s->component, id)) espmod_pool_release(pool(), slot);
}

/* a module that ended gives back what it still holds */
static void release_held(sim_t *s)
{
    s->mem_leaked = espmod_mem_used(&s->mem);
    for (unsigned i = 0; i < ESPMOD_HELD_MAX; i++) {
        if (s->held.e[i].state == ESPMOD_HELD_FREE) continue;
        if (s->held.e[i].state == ESPMOD_HELD_OWNED) s->leaked++;
        espmod_pool_release(pool(), s->held.e[i].slot);
        s->held.e[i].state = ESPMOD_HELD_FREE;
    }
    for (int k = 0; k < s->qn; k++) if (s->queue[k].request == 3) { espmod_pool_release(pool(), s->queue[k].pslot); s->queue[k].request = 0; s->queue[k].component = -1; }
}

sim_t *sim_new(espmod_handle_fn handler, uint32_t state_bytes)
{
    sim_t *s = calloc(1, sizeof(*s));
    s->handler = handler;
    espmod_held_init(&s->held, SIM_QUOTA);
    espmod_mem_init(&s->mem, SIM_MEM_QUOTA);
    s->state = calloc(1, state_bytes ? state_bytes : 1);
    for (int i = 0; i < PINS; i++) s->wire[i] = -1;
    s->table = (espmod_host){
        .abi_version = ESPMOD_ABI_VERSION, .size = sizeof(espmod_host),
        .features = ESPMOD_FEAT_SETTINGS | ESPMOD_FEAT_SPI | ESPMOD_FEAT_SPI_READY | ESPMOD_FEAT_I2C | ESPMOD_FEAT_ADC |
                    ESPMOD_FEAT_RGBLED | ESPMOD_FEAT_SERVICE | ESPMOD_FEAT_ALARM_FLEX | ESPMOD_FEAT_WIFI | ESPMOD_FEAT_PACKETS | ESPMOD_FEAT_MEMORY | ESPMOD_FEAT_SERIAL | ESPMOD_FEAT_MODULES,
        .user = s, .log = h_log, .now_ms = h_now, .respond = h_respond,
        .emit = h_emit, .call_service = h_service, .get_setting = h_get_setting,
        .set_setting = h_set_setting, .gpio_config = h_gpio_config, .gpio_read = h_gpio_read, .gpio_write = h_gpio_write,
        .irq_register = h_irq_register, .irq_unregister = h_irq_unregister, .irq_enable = h_irq_enable,
        .spi_open = h_spi_open, .spi_transfer = h_spi_transfer, .spi_close = h_spi_close, .i2c_transfer = h_i2c,
        .adc_read = h_adc, .rgbled_write = h_led,
        .alarm_set = h_alarm_set, .alarm_cancel = h_alarm_cancel, .connected = h_connected,
        .pkt_alloc = h_pkt_alloc, .pkt_buf = h_pkt_buf, .pkt_keep = h_pkt_keep, .pkt_free = h_pkt_free, .send = h_send,
        .pkt_info = h_pkt_info, .mem_alloc = h_mem_alloc, .mem_free = h_mem_free,
        .wifi_start = h_wifi_start, .wifi_stop = h_wifi_stop, .wifi_info = h_wifi_info, .wifi_scan = h_wifi_scan, .heap_info = h_heap_info, .heap_dump = h_heap_dump, .nv = h_nv, .wifi_config = h_wifi_config,
        .serial_open = h_serial_open, .serial_write = h_serial_write, .serial_close = h_serial_close,
        .device_info = h_device_info,
        .modules_info = h_modules_info, .module_start = h_module_start, .module_stop = h_module_stop, .module_manifest = h_module_manifest,
        .events_info = h_events_info, .reset = h_reset,
    };
    return s;
}

void sim_free(sim_t *s) { if (s) { release_held(s); for (int i = 0; i < 8; i++) if (s->blocks[i]) espmod_mem_free(&s->mem, s->blocks[i]); for (int i = 0; i < 8; i++) if (routes[i] == s) routes[i] = NULL; free(s->state); free(s->response); free(s); } }
const espmod_host *sim_host(sim_t *s) { return &s->table; }

static int dispatch(sim_t *s, int component, int method, int value, unsigned flags, const void *data, size_t len)
{
    if (s->ended) return 0;
    const espmod_call c = {.size = sizeof(c), .component = component, .method = method, .value = value, .flags = flags,
                           .data = data, .len = len};
    s->calls++;
    int rc = s->handler(&s->table, s->state, &c);
    s->result = rc;
    if (rc < 0 || rc == ESPMOD_RET_DONE || (component == ESPMOD_HOST_COMPONENT && method == ESPMOD_HOST_STOP)) s->ended = 1;
    return rc;
}

void sim_start(sim_t *s, const void *request, size_t len)
{
    if (s->qn >= QUEUE) return;
    call_t *c = &s->queue[s->qn++];
    memset(c, 0, offsetof(call_t, data));
    c->component = ESPMOD_HOST_COMPONENT; c->method = ESPMOD_HOST_START;
    c->len = len > INBOX ? INBOX : len;
    if (c->len) memcpy(c->data, request, c->len);
}

void sim_stop(sim_t *s) { enqueue(s, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_STOP, 0); }

int sim_post(sim_t *s, int component, int method, const void *data, size_t len)
{
    if (len > INBOX) return ESPMOD_ERR_SIZE;
    if (s->qn >= QUEUE) return ESPMOD_ERR_BUSY;
    call_t *c = &s->queue[s->qn++];
    memset(c, 0, offsetof(call_t, data));
    c->component = component; c->method = method; c->request = 1; c->len = len;
    if (len) memcpy(c->data, data, len);
    return 0;
}

int sim_request(sim_t *s, int component, int method, const void *data, size_t len, uint8_t *answer, size_t cap)
{
    int rc = sim_post(s, component, method, data, len);
    if (rc) return rc;
    s->queue[s->qn - 1].request = 2;   /* wants an answer */
    s->responded = 0; s->response_len = 0;
    sim_run(s, 0);
    if (s->qn) return ESPMOD_ERR_BUSY;
    size_t n = s->responded ? s->response_len : 0;
    if (n > cap) return ESPMOD_ERR_SIZE;
    if (n) memcpy(answer, s->response, n);
    return (int)n;
}

static void run_call(sim_t *s, const call_t *c)
{
    if (c->request == 3) {
        uint32_t h = espmod_held_add(&s->held, c->pslot, c->pstart, c->plen, ESPMOD_HELD_BORROWED);
        if (!h) { espmod_pool_release(pool(), c->pslot); return; }
        const int wants = c->id != 0;
        const espmod_call call = {.size = sizeof(call), .component = c->component, .method = c->method,
                                  .data = espmod_pool_slot(pool(), c->pslot) + c->pstart, .len = c->plen,
                                  .pkt = h, .from = c->from, .id = c->id, .flags = wants ? 1u : 0u};
        s->calls++;
        s->in_request = wants; s->responded = 0; s->response_len = 0;
        s->result = s->handler(&s->table, s->state, &call);
        s->in_request = 0;
        if (wants && s->responded) send_answer(s, c->from, c->component, c->method, c->id, s->response, s->response_len);
        if (s->result < 0 || s->result == ESPMOD_RET_DONE) s->ended = 1;
        espmod_held_entry *e = espmod_held_get(&s->held, h);
        if (e && e->state == ESPMOD_HELD_BORROWED) { espmod_held_take(&s->held, h); espmod_pool_release(pool(), c->pslot); }
        if (s->ended) release_held(s);
        return;
    }
    if (c->request) {
        s->in_request = 1; s->responded = 0;
        dispatch(s, c->component, c->method, 0, c->request == 2 ? 1 : 0, c->data, c->len);
        s->in_request = 0;
    } else {
        dispatch(s, c->component, c->method, c->value, 0, c->data, c->len);
    }
}

static void drain(sim_t *s)
{
    while (s->qn && !s->ended) {
        call_t c = s->queue[0];
        memmove(s->queue, s->queue + 1, --s->qn * sizeof(call_t));
        run_call(s, &c);
        if (s->ended) release_held(s);
    }
    if (s->ended) { release_held(s); s->qn = 0; }
}

static void fire_alarm(sim_t *s, int i)
{
    alarm_t *a = &s->alarms[i];
    if ((a->flags & ESPMOD_ALARM_CONNECTED) && !s->connected) { a->waiting_net = 1; a->fire = UINT64_MAX; return; }
    uint64_t late = s->now > a->due ? s->now - a->due : 0;
    int component = a->component, method = a->method;
    dispatch(s, component, method, late > 0x7fffffff ? 0x7fffffff : (int)late, 0, NULL, 0);
    if (s->ended || !a->used) return;
    if (!a->interval) { a->used = 0; return; }
    a->due += a->interval;
    uint32_t left = a->due > s->now ? (uint32_t)(a->due - s->now) : 0;
    a->fire = s->now + fire_delay(s, left, a->window, a->flags);
}

void sim_run(sim_t *s, uint32_t ms)
{
    const uint64_t end = s->now + ms;
    for (;;) {
        drain(s);
        if (s->ended) return;
        int next = -1;
        for (int i = 0; i < ALARMS; i++)
            if (s->alarms[i].used && !s->alarms[i].waiting_net && (next < 0 || s->alarms[i].fire < s->alarms[next].fire)) next = i;
        if (next < 0 || s->alarms[next].fire > end) break;
        if (s->alarms[next].fire > s->now) s->now = s->alarms[next].fire;
        fire_alarm(s, next);
    }
    s->now = end;
}

int sim_ended(sim_t *s) { return s->ended; }
int sim_result(sim_t *s) { return s->result; }
uint64_t sim_now(sim_t *s) { return s->now; }
uint32_t sim_calls(sim_t *s) { return s->calls; }
int sim_active_alarms(sim_t *s) { int n = 0; for (int i = 0; i < ALARMS; i++) n += s->alarms[i].used; return n; }

void sim_set_connected(sim_t *s, int connected)
{
    if (s->connected == connected) return;
    s->connected = connected;
    enqueue(s, ESPMOD_HOST_COMPONENT, ESPMOD_HOST_NETWORK, connected);
    if (!connected) return;
    for (int i = 0; i < ALARMS; i++) {
        alarm_t *a = &s->alarms[i];
        if (a->used && a->waiting_net) { a->waiting_net = 0; a->due = s->now; a->fire = s->now + fire_delay(s, 0, a->window, a->flags); }
    }
}

void sim_set_window(sim_t *s, uint32_t first_ms, uint32_t period_ms) { s->window_first = s->now + first_ms; s->window_period = period_ms; }
void sim_wire(sim_t *s, int from, int to) { s->wire[from] = to; }
void sim_drive(sim_t *s, int pin, int level) { drive(s, pin, level); }

int sim_event_count(sim_t *s) { return s->nevents; }
int sim_event(sim_t *s, int i, uint16_t *id, char *text, size_t cap)
{
    if (i < 0 || i >= s->nevents) return -1;
    if (id) *id = s->events[i].id;
    if (text && cap) { snprintf(text, cap, "%s", s->events[i].data); }
    return s->events[i].type;
}
int sim_find_event(sim_t *s, uint16_t id, const char *sub)
{
    for (int i = 0; i < s->nevents; i++)
        if (s->events[i].id == id && (!sub || strstr((char *)s->events[i].data, sub))) return i;
    return -1;
}
int sim_log_count(sim_t *s) { return s->nlogs; }
const char *sim_log(sim_t *s, int i) { return i >= 0 && i < s->nlogs ? s->logs[i] : ""; }
int sim_spi_transfers(sim_t *s) { return s->spi_transfers; }
