/* SPDX-License-Identifier: Apache-2.0 */
/* Run mod_hello and mod_test (the same sources that become flash images) against the simulated host: virtual time,
 * so alarms of a second cost nothing. */
#include <stdio.h>
#include <string.h>
#include "sim_host.h"

extern int hello_handle(const espmod_host *, void *, const espmod_call *);
extern int test_handle(const espmod_host *, void *, const espmod_call *);

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static void dump_events(sim_t *s)
{
    for (int i = 0; i < sim_event_count(s); i++) {
        uint16_t id; char t[80];
        sim_event(s, i, &id, t, sizeof(t));
        printf("  event 0x%04x %s\n", id, t);
    }
}

#define STATE 64
static sim_t *start_test(const char *request)
{
    sim_t *s = sim_new(test_handle, STATE);
    sim_start(s, request, strlen(request));
    return s;
}

static void test_hello(void)
{
    sim_t *s = sim_new(hello_handle, 0);
    sim_start(s, "hi", 2);
    sim_run(s, 0);
    CHECK(sim_ended(s) && sim_result(s) == ESPMOD_RET_DONE);
    CHECK(strstr(sim_log(s, 0), "hello from mod_hello"));
    CHECK(sim_find_event(s, 1, "hi") == 0);
    sim_free(s);
}

static void test_selftest(void)
{
    sim_t *s = start_test("selftest");
    sim_run(s, 0);
    if (sim_result(s) < 0) dump_events(s);
    CHECK(sim_ended(s) && sim_result(s) == ESPMOD_RET_DONE);
    CHECK(sim_find_event(s, 0x7eff, "fail=0") >= 0);
    for (uint16_t id = 0x7e00; id < 0x7e50; id++) CHECK(sim_find_event(s, id, "FAIL") < 0);
    CHECK(sim_event_count(s) > 35);
    CHECK(sim_active_alarms(s) == 0);
    sim_free(s);
}

static void test_serve_request_stop(void)
{
    sim_t *s = start_test("serve");
    sim_run(s, 10);
    CHECK(!sim_ended(s));
    static const uint8_t one[] = {0xa1, 0x01, 0x63, 'o', 'n', 'e'}, two[] = {0xa1, 0x01, 0x63, 't', 'w', 'o'};
    CHECK(sim_post(s, 2, 1, one, sizeof(one)) == 0);
    sim_run(s, 1);
    CHECK(sim_find_event(s, 0x7e10, "one") >= 0);
    uint8_t answer[32];
    int n = sim_request(s, 2, 1, two, sizeof(two), answer, sizeof(answer));
    CHECK(n == 6 && !memcmp(answer, two, 6));
    n = sim_request(s, 2, 2, NULL, 0, answer, sizeof(answer));     /* {1: calls, 2: mode} */
    CHECK(n > 3 && answer[0] == 0xa2);
    uint32_t calls = sim_calls(s);
    sim_run(s, 3600 * 1000);                                        /* an hour of idle: the module is not called */
    CHECK(sim_calls(s) == calls);
    sim_stop(s);
    sim_run(s, 0);
    CHECK(sim_ended(s) && sim_result(s) == 0);
    CHECK(sim_find_event(s, 0x7eff, "stopped") >= 0);
    sim_free(s);
}

static void test_alarms(void)
{
    sim_t *s = start_test("alarm");
    sim_run(s, 100);
    if (!sim_ended(s) || sim_result(s) != ESPMOD_RET_DONE) dump_events(s);
    CHECK(sim_ended(s) && sim_result(s) == ESPMOD_RET_DONE);
    CHECK(sim_find_event(s, 0x7eff, "fail=0") >= 0);
    sim_free(s);
}

static void test_gpio_loop(void)
{
    sim_t *s = start_test("gpio 4 5");
    sim_wire(s, 4, 5);
    sim_run(s, 200);
    if (sim_result(s) < 0 || !sim_ended(s)) dump_events(s);
    CHECK(sim_ended(s) && sim_result(s) == ESPMOD_RET_DONE);
    CHECK(sim_find_event(s, 0x7e00 + 5, "irq-event ok") >= 0);
    sim_free(s);
    /* no wire: the checks must notice */
    s = start_test("gpio 4 5");
    sim_run(s, 200);
    CHECK(sim_ended(s) && sim_result(s) < 0);
    sim_free(s);
}

static void test_spi_loop(void)
{
    sim_t *s = start_test("spi 1 2 3 4");
    sim_run(s, 0);
    CHECK(sim_ended(s) && sim_result(s) == ESPMOD_RET_DONE);
    CHECK(sim_spi_transfers(s) >= 1);
    sim_free(s);
}

/* Alarm rules of the host, driven through the host table (a module that only schedules). */
typedef struct { int fired; int last_late; uint64_t at[8]; } probe_t;
static int probe_handle(const espmod_host *h, void *state, const espmod_call *c)
{
    probe_t *p = state;
    if (c->component == ESPMOD_HOST_COMPONENT) {
        if (c->method == ESPMOD_HOST_START) {
            /* method = scenario: 1 flexible, 2 connected, 3 periodic */
            int scenario = c->len ? c->data[0] - '0' : 0;
            if (scenario == 1) h->alarm_set(h->user, 1000, 5000, 0, ESPMOD_ALARM_FLEXIBLE, 9, 1);
            if (scenario == 2) h->alarm_set(h->user, 100, 0, 0, ESPMOD_ALARM_CONNECTED, 9, 2);
            if (scenario == 3) h->alarm_set(h->user, 100, 0, 100, ESPMOD_ALARM_EXACT, 9, 3);
            if (scenario == 4) h->alarm_set(h->user, 1000, 1000, 0, ESPMOD_ALARM_FLEXIBLE, 9, 1);
        }
        return 0;
    }
    if (p->fired < 8) p->at[p->fired] = h->now_ms(h->user);
    p->fired++;
    p->last_late = c->value;
    return 0;
}

static void test_alarm_rules(void)
{
    sim_t *s = sim_new(probe_handle, sizeof(probe_t));
    /* flexible with a window at 3 s: fires there, not at 1 s */
    sim_start(s, "1", 1);
    sim_set_window(s, 3000, 10000);
    sim_run(s, 0);
    sim_run(s, 2999);
    CHECK(sim_calls(s) == 1);          /* only START so far */
    sim_run(s, 2);
    CHECK(sim_calls(s) == 2);
    sim_free(s);

    /* flexible with the window beyond the range: fires as late as allowed (delay + window) */
    s = sim_new(probe_handle, sizeof(probe_t));
    sim_start(s, "4", 1);
    sim_set_window(s, 60000, 60000);
    sim_run(s, 1999);
    CHECK(sim_calls(s) == 1);
    sim_run(s, 2);
    CHECK(sim_calls(s) == 2);
    sim_free(s);

    /* no window known: a flexible alarm is exact */
    s = sim_new(probe_handle, sizeof(probe_t));
    sim_start(s, "1", 1);
    sim_run(s, 999);
    CHECK(sim_calls(s) == 1);
    sim_run(s, 2);
    CHECK(sim_calls(s) == 2);
    sim_free(s);

    /* connected: held while offline, delivered after connectivity returns (plus HOST_NETWORK) */
    s = sim_new(probe_handle, sizeof(probe_t));
    sim_start(s, "2", 1);
    sim_run(s, 5000);
    CHECK(sim_calls(s) == 1 && sim_active_alarms(s) == 1);
    sim_set_connected(s, 1);
    sim_run(s, 1);
    CHECK(sim_calls(s) == 3);          /* START, NETWORK, the alarm */
    CHECK(sim_active_alarms(s) == 0);
    sim_free(s);

    /* periodic: re-armed from the schedule, ten calls in a second plus START */
    s = sim_new(probe_handle, sizeof(probe_t));
    sim_start(s, "3", 1);
    sim_run(s, 1000);
    CHECK(sim_calls(s) == 11);
    sim_free(s);
}

int main(void)
{
    test_hello();
    test_selftest();
    test_serve_request_stop();
    test_alarms();
    test_gpio_loop();
    test_spi_loop();
    test_alarm_rules();
    puts(failures ? "FAILED" : "modules ok");
    return failures != 0;
}
