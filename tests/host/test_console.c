/* SPDX-License-Identifier: Apache-2.0 */
/* mod_console against the simulated host: the lines of the serial port, the module calls, and the parse/render halves. */
#include <stdio.h>
#include <string.h>
#include "sim_host.h"
#include "espmod_cbor.h"

extern int console_handle(const espmod_host *, void *, const espmod_call *);
extern int test_handle(const espmod_host *, void *, const espmod_call *);

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static sim_t *s;

/* parse: returns the response map in `out` */
static int parse(const char *line, uint8_t *out, size_t cap)
{
    uint8_t req[200];
    em_cbor_w w;
    em_cbor_init(&w, req, sizeof(req));
    em_cbor_map(&w, 1); em_cbor_uint(&w, 1); em_cbor_text(&w, line, strlen(line));
    return sim_request(s, 20, 1, req, (size_t)em_cbor_done(&w), out, cap);
}

static int local_text(const uint8_t *r, size_t n, char *text, size_t cap)
{
    const uint8_t *p; size_t pn;
    if (em_cbor_get_str(r, n, 12, EM_CBOR_TEXT, &p, &pn)) return -1;
    snprintf(text, cap, "%.*s", (int)pn, (const char *)p);
    return 0;
}

static void check_request(const char *line, int comp, int method, const char *fields_hex)
{
    uint8_t r[512];
    int n = parse(line, r, sizeof(r));
    int64_t c = -1, m = -1;
    em_cbor_get_int(r, (size_t)n, 1, &c);
    em_cbor_get_int(r, (size_t)n, 2, &m);
    em_cbor_r f;
    char hex[200] = "";
    if (n > 0 && em_cbor_get(r, (size_t)n, 5, &f) == 0) {
        em_cbor_r g = f; em_cbor_item it;
        em_cbor_next(&g, &it);
        size_t body = f.len - g.len;   /* head only; render the whole map: skip to find its end */
        em_cbor_r k = f; em_cbor_skip(&k);
        body = f.len - k.len;
        for (size_t i = 0; i < body; i++) sprintf(hex + 2 * i, "%02x", f.p[i]);
    }
    if (c != comp || m != method || strcmp(hex, fields_hex)) {
        char t[256] = "";
        if (n > 0) local_text(r, (size_t)n, t, sizeof(t));
        printf("FAIL line \"%s\": got %lld.%lld fields %s local \"%s\", want %d.%d %s\n", line, (long long)c, (long long)m, hex, t, comp, method, fields_hex);
        failures++;
    }
}

static void check_local(const char *line, const char *want)
{
    uint8_t r[1100];
    int n = parse(line, r, sizeof(r));
    char t[1100];
    CHECK(n > 0 && local_text(r, (size_t)n, t, sizeof(t)) == 0);
    if (n > 0 && !strstr(t, want)) { printf("FAIL line \"%s\": \"%s\" lacks \"%s\"\n", line, t, want); failures++; }
}

static void check_render(const uint8_t *res, size_t rn, int comp, int method, const char *want)
{
    uint8_t req[600], ans[1100];
    em_cbor_w w;
    em_cbor_init(&w, req, sizeof(req));
    em_cbor_map(&w, 3);
    em_cbor_uint(&w, 1); em_cbor_uint(&w, (unsigned)comp);
    em_cbor_uint(&w, 2); em_cbor_uint(&w, (unsigned)method);
    em_cbor_uint(&w, 6); em_cbor_put(&w, res, rn);
    int n = sim_request(s, 20, 2, req, (size_t)em_cbor_done(&w), ans, sizeof(ans));
    const uint8_t *t; size_t tn;
    char text[1100] = "";
    if (n > 0 && em_cbor_get_str(ans, (size_t)n, 1, EM_CBOR_TEXT, &t, &tn) == 0) snprintf(text, sizeof(text), "%.*s", (int)tn, (const char *)t);
    if (strcmp(text, want)) { printf("FAIL render: got \"%s\" want \"%s\"\n", text, want); failures++; }
}

/* a typed line, as the serial port delivers it: a packet for the console. The answer is what the console writes to the port,
 * after the packets between it and the module have run. It must be exactly `want` (when it ends with a newline) or contain it. */
static sim_t *module_sim;
static void pump(void)
{
    for (int i = 0; i < 4; i++) {
        sim_run(s, 0);
        if (module_sim) sim_run(module_sim, 0);
    }
}

static void check_line(const char *line, const char *want)
{
    sim_serial_clear();
    int rc = sim_send(s, 20, 3, 0, line, strlen(line));
    pump();
    int exact = strlen(want) && want[strlen(want) - 1] == '\n';
    if (rc || (exact ? strcmp(sim_serial_out, want) : !strstr(sim_serial_out, want))) {
        printf("FAIL line \"%s\": got \"%s\" (send %d), want \"%s\"\n", line, sim_serial_out, rc, want);
        failures++;
    }
}

int main(void)
{
    s = sim_new(console_handle, 32);
    sim_set_component(s, 20);
    sim_start(s, "", 0);
    sim_run(s, 0);

    check_request("test.echo value=hi", 2, 1, "a1" "01" "626869");
    check_request("test.echo value=\"two words\"", 2, 1, "a1" "01" "6974776f20776f726473");
    check_request("test.echo @1=7", 2, 1, "a1" "01" "6137");
    check_local("nosuch", "unknown method");
    check_local("status bogus=1", "unknown method");

    /* answers */
    uint8_t r[400];
    em_cbor_w w;
    em_cbor_init(&w, r, sizeof(r));
    em_cbor_map(&w, 1); em_cbor_uint(&w, 1); em_cbor_text(&w, "hi there", 8);
    check_render(r, (size_t)em_cbor_done(&w), 2, 1, "value=\"hi there\"\n");
    em_cbor_init(&w, r, sizeof(r));
    em_cbor_map(&w, 0);
    check_render(r, (size_t)em_cbor_done(&w), 2, 3, "ok\n");
    em_cbor_init(&w, r, sizeof(r));
    em_cbor_map(&w, 2); em_cbor_uint(&w, 1); em_cbor_int(&w, -68); em_cbor_uint(&w, 77); em_cbor_bytes(&w, "\x0a\x0b", 2);
    check_render(r, (size_t)em_cbor_done(&w), 99, 99, "@1=-68 @77=hex:0a0b\n");

    /* the console as the serial port's owner: lines in, text out */
    CHECK(sim_serial_component == 20 && sim_serial_method == 3);
    em_cbor_w mw;
    em_cbor_init(&mw, sim_modules_cbor, sizeof(sim_modules_cbor));
    em_cbor_array(&mw, 2);
    em_cbor_map(&mw, 3); em_cbor_uint(&mw, 1); em_cbor_text(&mw, "hello", 5); em_cbor_uint(&mw, 2); em_cbor_uint(&mw, 1); em_cbor_uint(&mw, 3); em_cbor_bool(&mw, 0);
    em_cbor_map(&mw, 4); em_cbor_uint(&mw, 1); em_cbor_text(&mw, "test", 4); em_cbor_uint(&mw, 2); em_cbor_uint(&mw, 2); em_cbor_uint(&mw, 3); em_cbor_bool(&mw, 1); em_cbor_uint(&mw, 5); em_cbor_uint(&mw, 7);
    sim_modules_len = (size_t)em_cbor_done(&mw);
    em_cbor_init(&mw, sim_events_cbor, sizeof(sim_events_cbor));
    em_cbor_array(&mw, 1);
    em_cbor_map(&mw, 3); em_cbor_uint(&mw, 1); em_cbor_text(&mw, "test", 4); em_cbor_uint(&mw, 2); em_cbor_uint(&mw, 32511); em_cbor_uint(&mw, 3); em_cbor_text(&mw, "pass=28 fail=0", 14);
    sim_events_len = (size_t)em_cbor_done(&mw);

    check_line("status", "name=hello tag=1 installed=false\nname=test tag=2 installed=true version=7\n");
    check_line("events", "module=test id=32511 pass=28 fail=0\n");
    check_line("start test serve it", "ok\n");
    CHECK(!strcmp(sim_started, "test") && !strcmp(sim_start_request, "serve it"));
    check_line("start nosuch", "error: unknown module\n");
    check_line("start", "error: name is required\n");
    check_line("stop test", "ok\n");
    CHECK(!strcmp(sim_stopped, "test"));
    check_line("reset", "ok\n");
    CHECK(sim_reset_called);
    check_line("", "");
    check_line("help", "status | start NAME");
    check_line("nosuch", "error: unknown method");
    check_line("test.echo value=hi", "error: module not running (start it)\n");
    sim_t *test = sim_new(test_handle, 64);
    sim_start(test, "serve", 5);
    sim_set_component(test, 2);
    module_sim = test;
    pump();
    check_line("test.echo value=hello", "value=hello\n");
    check_line("test.echo hello there", "value=\"hello there\"\n");
    check_line("wifi", "state=off saved_ssid=\"(flash network)\" psk=unset\n");
    check_line("wifi ssid=lab psk=\"two words\"", "saved_ssid=\"lab\" psk=set");
    check_line("wifi start", "state=up ssid=\"lab\" ip=192.168.49.7 rssi=-40 saved_ssid=\"lab\" psk=set\n");
    CHECK(!strcmp(sim_ssid, "lab") && !strcmp(sim_psk, "two words") && sim_wifi_on);
    check_line("wifi stop", "state=off");
    check_line("heap", "free=41000 largest=30000 min_free=28000 total=230000 internal_free=38000 internal_largest=26000\n");
    check_line("heap regions", "ok (in the device log)\n");
    check_line("heap bogus", "error: heap [regions");
    check_line("nv wifi.rx_static", "wifi.rx_static is unknown or not set\n");
    check_line("nv wifi.rx_static=12", "wifi.rx_static set (applies at the next start)\n");
    check_line("nv wifi.rx_static", "wifi.rx_static=12\n");
    check_line("nv bogus", "bogus is unknown or not set\n");
    check_line("wifi scan", "ssid=\"costin\" ch=6 rssi=-41 auth=3 bssid=10:bd:a3:ac:5a:21\nssid=\"\" ch=11 rssi=-80 auth=0 bssid=01:02:03:04:05:06\n");
    check_line("wifi bogus", "error: wifi [ssid=NAME]");
    check_line("wifi ssid=", "saved_ssid=\"(flash network)\"");
    check_line("loader start test", "ok\n");
    CHECK(sim_loader_on && !strcmp(sim_loader_name, "test"));
    check_line("loader start test", "error: busy");
    check_line("loader stop", "ok\n");
    CHECK(!sim_loader_on);
    check_line("loader start", "error: loader start NAME");
    /* the console waits for nothing: two requests in flight, answers in any order, and messages nobody asked for */
    sim_serial_clear();
    CHECK(sim_send(s, 20, 3, 0, "test.echo value=one", 19) == 0);
    CHECK(sim_send(s, 20, 3, 0, "test.echo value=two", 19) == 0);
    pump();
    CHECK(strstr(sim_serial_out, "value=one\n") && strstr(sim_serial_out, "value=two\n"));
    static const uint8_t a1[] = {0xa1, 0x01, 0x61, 'a'}, b1[] = {0xa1, 0x01, 0x61, 'b'};
    sim_serial_clear();
    CHECK(sim_send_from(s, 2, 2, 1, 0, a1, sizeof(a1)) == 0);          /* from the module, no request: a line it chose to write */
    CHECK(sim_send_from(s, 2, 2, 1, 77, b1, sizeof(b1)) == 0);         /* and another, with an id of its own */
    pump();
    CHECK(!strcmp(sim_serial_out, "value=a\nvalue=b\n"));

    /* the manifest of the installed module is its own word on its methods */
    em_cbor_w mf;
    em_cbor_init(&mf, sim_manifest, sizeof(sim_manifest));
    em_cbor_map(&mf, 3);
    em_cbor_uint(&mf, 1); em_cbor_text(&mf, "test", 4);
    em_cbor_uint(&mf, 2); em_cbor_uint(&mf, 2);
    em_cbor_uint(&mf, 3); em_cbor_array(&mf, 2);
    em_cbor_map(&mf, 2); em_cbor_uint(&mf, 1); em_cbor_uint(&mf, 1); em_cbor_uint(&mf, 2); em_cbor_text(&mf, "echo", 4);
    em_cbor_map(&mf, 2); em_cbor_uint(&mf, 1); em_cbor_uint(&mf, 2); em_cbor_uint(&mf, 2); em_cbor_text(&mf, "info", 4);
    sim_manifest_len = (size_t)em_cbor_done(&mf);
    check_line("test.echo value=listed", "value=listed\n");
    check_line("test.pkt_echo", "does not list method 20 in its manifest");       /* in the catalog, not in this image */
    sim_manifest_len = 0;
    check_line("test.pkt_echo", "");                                              /* no manifest: the catalog decides */

    /* a module that never answers costs the console nothing */
    sim_free(test);
    module_sim = sim_new(test_handle, 64);
    sim_set_component(module_sim, 2);                                 /* never started or run */
    sim_serial_clear();
    CHECK(sim_send(s, 20, 3, 0, "test.echo value=x", 17) == 0);
    sim_run(s, 0);
    CHECK(sim_serial_len == 0);
    check_line("status", "name=hello");                                /* still answering lines */
    sim_free(module_sim);
    module_sim = NULL;
    sim_stop(s);
    sim_run(s, 0);
    CHECK(sim_serial_component == -1);

    sim_free(s);
    puts(failures ? "FAILED" : "console ok");
    return failures != 0;
}
