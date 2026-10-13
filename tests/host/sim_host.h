/* SPDX-License-Identifier: Apache-2.0 */
/* A host that runs on a PC: the same event model as components/espmod_host (one handler call at a time, alarms,
 * interrupts, requests), with virtual time and pins and an SPI bus made of memory. A module's handler is tested
 * without a board (tests/host/test_modules.c): nothing sleeps, `sim_run` advances the clock. */
#pragma once
#include <stddef.h>
#include <stdint.h>
#include "espmod.h"

typedef struct sim sim_t;

extern char sim_ssid[40], sim_psk[72], sim_loader_name[40];   /* what wifi_start and loader_start were given */
extern int sim_wifi_on, sim_loader_on;
/* The console's calls */
extern int sim_serial_component, sim_serial_method, sim_reset_called;
extern char sim_serial_out[4096];                                 /* what the module wrote with serial_write */
extern size_t sim_serial_len;
void sim_serial_clear(void);
extern char sim_started[40], sim_start_request[80], sim_stopped[40];
extern uint8_t sim_modules_cbor[600], sim_events_cbor[600];   /* what modules_info and events_info return */
extern size_t sim_modules_len, sim_events_len;
extern uint8_t sim_manifest[200];        /* what module_manifest("test") returns (empty: the module has none) */
extern size_t sim_manifest_len;

/* Packets (abi/API.md PKT): the same pool and held-table code as the C host, with a small pool of 8 packets */
typedef struct { uint32_t from; int component, method; uint64_t id; size_t len; uint8_t data[256]; } sim_msg_t;
extern sim_msg_t sim_main_msgs[8];     /* what modules sent to ADDR_MAIN */
extern int sim_main_n;
void sim_main_clear(void);
void sim_set_component(sim_t *s, int component);                      /* this module is the target of send(dst = component) */
int sim_send(sim_t *s, int component, int method, uint64_t id, const void *data, size_t len);   /* from the application */
int sim_send_from(sim_t *s, uint32_t from, int component, int method, uint64_t id, const void *data, size_t len);   /* as if module `from` sent it */
int sim_pool_available(void);
unsigned sim_held_count(sim_t *s);                                     /* packets the module owns */
unsigned sim_mem_leaked(sim_t *s);                                     /* bytes not given back when it ended */
unsigned sim_leaked(sim_t *s);                                         /* owned when it ended */

sim_t *sim_new(espmod_handle_fn handler, uint32_t state_bytes);
void sim_free(sim_t *s);
const espmod_host *sim_host(sim_t *s);

/* Application side. Calls are queued and run by sim_run. */
void sim_start(sim_t *s, const void *request, size_t len);            /* HOST_START */
void sim_stop(sim_t *s);                                              /* HOST_STOP */
int sim_post(sim_t *s, int component, int method, const void *data, size_t len);   /* a request, no answer wanted */
/* A request with an answer (runs the simulation until it is handled). Returns the answer length or a negative error. */
int sim_request(sim_t *s, int component, int method, const void *data, size_t len, uint8_t *answer, size_t cap);
void sim_run(sim_t *s, uint32_t ms);                                  /* handle queued calls and alarms for `ms` of virtual time */
int sim_ended(sim_t *s);                                              /* the instance finished */
int sim_result(sim_t *s);                                             /* what the last handler call returned */
uint64_t sim_now(sim_t *s);
void sim_set_connected(sim_t *s, int connected);                      /* HOST_NETWORK + CONNECTED alarms */
void sim_set_window(sim_t *s, uint32_t first_ms, uint32_t period_ms); /* sleepy mode: wake windows (0: none) */
void sim_wire(sim_t *s, int from_pin, int to_pin);                    /* an output on from_pin drives to_pin */
void sim_drive(sim_t *s, int pin, int level);                         /* the outside world changes a pin */
int sim_active_alarms(sim_t *s);
uint32_t sim_calls(sim_t *s);                                         /* handler calls so far */

/* What the module produced. */
int sim_event_count(sim_t *s);
int sim_event(sim_t *s, int index, uint16_t *id, char *text, size_t cap);   /* returns value type */
int sim_find_event(sim_t *s, uint16_t id, const char *substring);           /* index or -1 */
int sim_log_count(sim_t *s);
const char *sim_log(sim_t *s, int index);
int sim_spi_transfers(sim_t *s);
