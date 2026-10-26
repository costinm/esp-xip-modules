/* SPDX-License-Identifier: Apache-2.0 */
/* The sleep owner of the runtime (the `power` component).
 *
 * WHAT THIS IS
 *   DMesh's explicit timer light sleep (fw/dmesh-fw-transport: power_esp.rs enter_timer_light_sleep, and the radio / UART steps of main_runtime.rs
 *   maybe_enter_sleep), offered as a request that a module or the console makes. The POLICY (when to sleep, for how long, in which phase of the NAN discovery
 *   window, what to announce, what to do with power save) belongs to the caller - in practice mod_nan, which owns "coordinated sleep". This file owns only the
 *   SEQUENCE, because the sequence took a very long time to get working on the boards and most other orderings fail. Do not reorder it without testing on a board.
 *
 * THE SEQUENCE (one cycle, on the host worker task)
 *   1. Stop the radio the way a Wi-Fi epoch ends (xip_wifi.c wifi_down / Rust wifi::power_suspend): ESP-NOW callback goes inert, the station DISCONNECTS
 *      (some access points dislike a reconnect without a disconnect first), esp_wifi_stop, the STA netif is destroyed; the initialized driver is kept (cheaper
 *      than re-allocating its buffers every wake). On the CLASSIC ESP32 the driver is released too (esp_wifi_deinit): an initialized-but-stopped driver
 *      makes esp_light_sleep_start() return immediately there.
 *   2. Arm the timer wake. (The UART wake was armed once by the serial driver, xip_serial.c: esp_sleep_enable_uart_wakeup, so a byte on the UART ends the sleep early.)
 *   3. esp_light_sleep_start(); measure the real duration; count the sleep as entered, or as rejected when it returns an error at once (a power-management lock).
 *   4. Re-arm the UART receive: a wake can leave its interrupt disabled although the driver and the reader task are installed.
 *   5. Bring the radio back with the SAME settings (the DMesh bring-up again, driven by the wanted Wi-Fi intent); measure how long it took (info.last_resume_us).
 *   6. Deliver HOST_WAKE to the modules that have an ALARM_WINDOW alarm. Its value is the wake cause (4 timer, 8 UART, 7 GPIO).
 *   A rejected sleep is NOT a wake: the radio is restored once and the rest of the request is dropped - a PM-lock rejection must never become a stop/start loop.
 *   A wake that is not the timer (a person typing on the UART, a peer's packet) means "swap to active": the rest of the request is dropped and the module,
 *   which sees the cause in HOST_WAKE, decides what follows (NAN: restore active roles for the shared five-second activity grace).
 *   Between two cycles of one request there is an awake time (a timer that queues the next cycle) so modules run.
 *
 * USB-SERIAL/JTAG: coordinated NAN sleep is vetoed while a host is attached by
 * default. NV usb.sleep=1 explicitly opts into USJ PHY pull-up removal before
 * physical sleep. The runtime leaves it detached across timer/DW wakes; only
 * NAN transport.set selecting serial on restores the prior PHY state. Explicit
 * serial on also vetoes USB sleep until policy selects serial off again.
 * This is experimental: prior automatic re-enumeration around light sleep left
 * bench hosts with missing/half-enumerated ports. Do not infer host recovery or
 * power savings from a successful build or queued sleep. The standalone power
 * diagnostic retains its soft path with the default attached-host policy.
 *
 * Runs on the host worker; the C Wi-Fi owner supplies suspend/resume. */
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include "sdkconfig.h"
#include "esp_heap_caps.h"
#include "esp_pm.h"
#include "esp_sleep.h"
#include "esp_timer.h"
#include "driver/uart.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "xip_runtime.h"
#include "xip_wifi.h"
#include "xip_cbor.h"
#include "internal.h"

#define MIN_SLEEP_MS 10
#define DEFAULT_AWAKE_MS 50
/* DMesh's active policy: 80..160 MHz, light sleep only when asked explicitly */
#define PM_MIN_MHZ 80
#define PM_MAX_MHZ 160

/* BEGIN ACTIVITY WINDOW: monotonic milliseconds, no task/timer allocation. */
static atomic_uint active_until_ms, activity_seq;
static _Atomic(TaskHandle_t) sleep_task;
static void signal_activity(void)
{
    atomic_fetch_add(&activity_seq, 1);
    TaskHandle_t task = atomic_load(&sleep_task);
    if (task) xTaskNotifyGive(task);
    xip_host_wake_window_value(8);
}
static uint32_t activity_now_ms(void) { return (uint32_t)(esp_timer_get_time() / 1000); }
uint32_t xip_power_active_remaining_ms(void)
{
    uint32_t until = atomic_load(&active_until_ms), now = activity_now_ms();
    int32_t remaining = (int32_t)(until - now);
    if (until && remaining > 0) return (uint32_t)remaining;
    unsigned expected = until;
    (void)atomic_compare_exchange_strong(&active_until_ms, &expected, 0);
    return 0;
}
void xip_power_activate(uint32_t ms)
{
    if (!ms) return;
    if (ms > INT32_MAX) ms = INT32_MAX;
    uint32_t now = activity_now_ms(), next = now + ms;
    if (!next) next = 1; /* zero is the inactive sentinel */
    unsigned current = atomic_load(&active_until_ms);
    for (;;) {
        if (current && (int32_t)(current - now) > 0 && (int32_t)(current - next) >= 0) break;
        if (atomic_compare_exchange_weak(&active_until_ms, &current, next)) break;
    }
    /* Publish the grace before waking the higher-priority policy worker. */
    signal_activity();
}
/* Temporary module work is separate from UART/explicit active-mode intent.
 * One wrap-safe monotonic deadline, no per-module allocations or vote table. */
static atomic_uint work_until_ms;
int xip_power_awake_for(uint32_t ms)
{
    if (ms > 300000) return XIP_ERR_ARG;
    const uint32_t now = activity_now_ms();
    unsigned current = atomic_load(&work_until_ms);
    if (ms) {
        uint32_t next = now + ms;
        if (!next) next = 1;
        for (;;) {
            if (current && (int32_t)(current - now) > 0 && (int32_t)(current - next) >= 0) break;
            if (atomic_compare_exchange_weak(&work_until_ms, &current, next)) { current = next; break; }
        }
    }
    return current && (int32_t)(current - now) > 0 ? (int32_t)(current - now) : 0;
}
void xip_power_clear_activity(void) { atomic_store(&active_until_ms, 0); }
/* END ACTIVITY WINDOW */

uint32_t esp_clk_cpu_freq(void);
static xip_power_radio radio;
static atomic_bool busy;
static atomic_uchar state_now;
static atomic_ushort left;
static atomic_uint duration_ms, awake_ms;
/* All power measurements have one owner; no module-sized idle diagnostics. */
static struct {
    atomic_uint attempts, entries, skipped, last_requested, last_duration, last_resume;
    atomic_uchar wake_cause, soft_now;
    atomic_bool physical;
} metrics;
static esp_timer_handle_t awake_timer;
static bool pm_done;
/* Admission is serialized on the module worker; only UART activity is concurrent. */
static uint32_t request_owner, request_activity, request_flags, request_wifi_generation;
static uint32_t current_wifi_generation(void) {
    xip_wifi_intent intent = {.size = sizeof(intent)}; uint32_t generation = 0;
    (void)xip_wifi_get_intent(&intent, &generation); return generation;
}
static uint64_t request_deadline;

static void trace_boundary(unsigned phase, unsigned cause)
{
    if (!(request_flags & XIP_POWER_TRACE_UART)) return;
    uint8_t out[80];
    xip_cbor_w w; xip_cbor_init(&w, out, sizeof(out));
    xip_cbor_map(&w, 11);
    xip_cbor_uint(&w, 1); xip_cbor_uint(&w, phase);
    xip_cbor_uint(&w, 2); xip_cbor_uint(&w, esp_timer_get_time());
    xip_cbor_uint(&w, 3); xip_cbor_uint(&w, cause);
    xip_cbor_uint(&w, 4); xip_cbor_uint(&w, atomic_load(&metrics.last_requested));
    xip_cbor_uint(&w, 5); xip_cbor_uint(&w, phase ? atomic_load(&metrics.last_duration) : 0);
    xip_cbor_uint(&w, 6); xip_cbor_uint(&w, phase ? atomic_load(&metrics.last_resume) : 0);
    xip_cbor_uint(&w, 7); xip_cbor_bool(&w, atomic_load(&metrics.soft_now));
    xip_cbor_uint(&w, 8); xip_cbor_bool(&w, phase && atomic_load(&metrics.physical));
    xip_cbor_uint(&w, 9); xip_cbor_uint(&w, atomic_load(&metrics.entries));
    xip_cbor_uint(&w, 10); xip_cbor_uint(&w, atomic_load(&metrics.skipped));
    xip_cbor_uint(&w, 11); xip_cbor_uint(&w, xip_host_network_sockets());
    int n = xip_cbor_done(&w);
    if (n > 0) (void)xip_serial_write_frame(250, out, (size_t)n);
}

int xip_power_cancel(uint32_t owner)
{
    if (request_owner != owner) return XIP_ERR_NOTFOUND;
    atomic_store(&left, 0);
    if (awake_timer) (void)esp_timer_stop(awake_timer);
    atomic_store(&busy, false);
    atomic_store(&state_now, XIP_POWER_IDLE);
    request_owner = 0;
    return XIP_OK;
}

void xip_power_set_radio(const xip_power_radio *r) { radio = r ? *r : (xip_power_radio){0}; }

/* BLE's part of a sleep (xip_ble.c): it goes outside the radio's suspend and resume, and it may veto. */
static const xip_power_ble *ble_hooks;
void xip_power_set_ble(const xip_power_ble *b) { ble_hooks = b; }

/* Frequency scaling needs the PM configured once; light sleep stays explicit (the timer sleep below), as DMesh's Active profile. */
static void configure_pm(void)
{
    if (pm_done) return;
    pm_done = true;
#if CONFIG_PM_ENABLE
    const esp_pm_config_t cfg = {.max_freq_mhz = PM_MAX_MHZ, .min_freq_mhz = PM_MIN_MHZ, .light_sleep_enable = false};
    (void)esp_pm_configure(&cfg);
#endif
}

static void rearm_uart(void)
{
    /* a GPIO/UART wake can leave the receive interrupt disabled although the driver and its reader task are installed */
    if (uart_is_driver_installed(UART_NUM_0)) {
        (void)uart_set_wakeup_threshold(UART_NUM_0, 3);
        (void)uart_enable_rx_intr(UART_NUM_0);
    }
}

static void post_next(void *arg)
{
    (void)arg;
    if (xip_power_post() < 0) {
        atomic_store(&left, 0);
        atomic_store(&busy, false);
        atomic_store(&state_now, XIP_POWER_IDLE);
        xip_host_wake_window_value(-1);
    }
}

/* One cycle, on the worker. UART RX continues on its own task. */
void xip_power_cycle(void)
{
    if (!atomic_load(&left)) { atomic_store(&state_now, XIP_POWER_IDLE); atomic_store(&busy, false); return; }
    configure_pm();
    const uint64_t relative_us = (uint64_t)atomic_load(&duration_ms) * 1000;
    const bool coordinated = request_deadline != 0;
    /* A stale queued request or activity since admission cannot shut down Wi-Fi. */
    if ((coordinated && (xip_serial_usb_sleep_blocked() || request_activity != atomic_load(&activity_seq) || xip_power_active_remaining_ms() || request_wifi_generation != current_wifi_generation())) || xip_power_awake_for(0)) {
        atomic_store(&left, 0); atomic_store(&busy, false);
        xip_host_wake_window_value(xip_power_active_remaining_ms() ? 8 : 4);
        return;
    }
    atomic_fetch_add(&metrics.attempts, 1);
    atomic_store(&metrics.last_requested, coordinated ? (uint32_t)(request_deadline > (uint64_t)esp_timer_get_time()
        ? request_deadline - esp_timer_get_time() : 0) : (uint32_t)relative_us);
    atomic_store(&metrics.soft_now, xip_serial_usb_sleep_blocked());
    atomic_store(&metrics.physical, false);
    atomic_store(&metrics.last_duration, 0);
    atomic_store(&metrics.last_resume, 0);
    if (ble_hooks && ble_hooks->suspend && ble_hooks->suspend() < 0) goto rejected;
    trace_boundary(0, 0);
    if (xip_serial_drain() < 0) {
        if (ble_hooks && ble_hooks->resume) (void)ble_hooks->resume();
        goto rejected;
    }
    const bool had_radio = radio.suspend && radio.suspend();
    atomic_store(&state_now, XIP_POWER_SLEEPING);
    const bool soft = atomic_load(&metrics.soft_now);
    esp_err_t rc = ESP_OK;
    if (!soft) xip_serial_sleep_suspend();
    const int64_t before = esp_timer_get_time();
    const uint64_t asked_us = coordinated
        ? (request_deadline > (uint64_t)before ? request_deadline - before : 0) : relative_us;
    uint8_t cause = 4;
    bool waited = false;
    /* Publish the waiter before checking activity, closing the soft-sleep RX race. */
    (void)ulTaskNotifyTake(pdTRUE, 0);
    atomic_store(&sleep_task, xTaskGetCurrentTaskHandle());
    if ((coordinated && (request_activity != atomic_load(&activity_seq) || xip_power_active_remaining_ms() || request_wifi_generation != current_wifi_generation())) || xip_power_awake_for(0)) {
        cause = xip_power_active_remaining_ms() ? 8 : 4;
    } else if (asked_us < MIN_SLEEP_MS * 1000) {
        rc = ESP_ERR_INVALID_STATE;
    } else if (soft) {
        waited = true;
        TickType_t ticks = pdMS_TO_TICKS((asked_us + 999) / 1000);
        if (ulTaskNotifyTake(pdTRUE, ticks ? ticks : 1)) cause = 8;
    } else {
        rc = esp_sleep_enable_timer_wakeup(asked_us);
        if (rc == ESP_OK) {
            waited = true;
            rc = esp_light_sleep_start();
            const uint32_t causes = esp_sleep_get_wakeup_causes();
            cause = 0;
            /* UART wins over a simultaneous timer: the user requested awake. */
            if (causes & (1u << ESP_SLEEP_WAKEUP_UART)) cause = 8;
            else for (uint8_t b = 1; b < 32 && !cause; b++) if (causes & (1u << b)) cause = b;
        }
    }
    atomic_store(&sleep_task, NULL);
    /* The UART wake edge may precede delivery of the first buffered byte.
     * Give the trusted local sender a full packet-receive grace immediately. */
    if (rc==ESP_OK && waited && cause==8) xip_serial_note_wake();
    atomic_store(&metrics.last_duration, (uint32_t)(esp_timer_get_time() - before));
    atomic_store(&metrics.wake_cause, cause);
    atomic_store(&metrics.physical, waited && !soft && rc == ESP_OK);
    if (rc == ESP_OK && waited) atomic_fetch_add(&metrics.entries, 1);
    else if (rc != ESP_OK) atomic_fetch_add(&metrics.skipped, 1);
    if (!soft) rearm_uart();
    atomic_store(&state_now, XIP_POWER_RESUMING);
    const int64_t resume_start = esp_timer_get_time();
    if (had_radio && radio.resume) radio.resume();
    if (ble_hooks && ble_hooks->resume) (void)ble_hooks->resume();
    atomic_store(&metrics.last_resume, (uint32_t)(esp_timer_get_time() - resume_start));
    trace_boundary(rc == ESP_OK ? 1 : 2, cause);
    if (rc != ESP_OK || cause != 4) atomic_store(&left, 0);
    else atomic_fetch_sub(&left, 1);
    /* Even a rejected request notifies the policy, so it can fall back to awake
     * instead of repeatedly stopping/starting the radio against a PM veto. */
    xip_host_wake_window_value(rc == ESP_OK ? cause : -1);
    if (!atomic_load(&left)) {
        atomic_store(&state_now, XIP_POWER_IDLE);
        atomic_store(&busy, false);
        return;
    }
    atomic_store(&state_now, XIP_POWER_AWAKE);
    if (!awake_timer) {
        const esp_timer_create_args_t args = {.callback = post_next, .dispatch_method = ESP_TIMER_TASK, .name = "xip-sleep"};
        if (esp_timer_create(&args, &awake_timer) != ESP_OK) { atomic_store(&left, 0); atomic_store(&busy, false); return; }
    }
    (void)esp_timer_start_once(awake_timer, (uint64_t)atomic_load(&awake_ms) * 1000);
    return;
rejected:
    atomic_fetch_add(&metrics.skipped, 1);
    trace_boundary(2, 0);
    atomic_store(&left, 0);
    atomic_store(&state_now, XIP_POWER_IDLE);
    atomic_store(&busy, false);
    xip_host_wake_window_value(-1);
}

int xip_power_request(const xip_power_sleep *req, uint32_t owner)
{
    if (!req || req->size < sizeof(*req) || req->duration_ms < MIN_SLEEP_MS) return XIP_ERR_ARG;
    if (xip_power_awake_for(0) || atomic_exchange(&busy, true)) return XIP_ERR_BUSY;
    request_owner = owner;
    request_deadline = 0;
    request_flags = 0;
    request_activity = atomic_load(&activity_seq);
    atomic_store(&duration_ms, req->duration_ms);
    atomic_store(&awake_ms, req->awake_ms ? req->awake_ms : DEFAULT_AWAKE_MS);
    atomic_store(&left, req->cycles ? req->cycles : 1);
    atomic_store(&state_now, XIP_POWER_AWAKE);
    int rc = xip_power_post();
    if (rc < 0) (void)xip_power_cancel(owner);
    return rc;
}

int xip_power_until(uint32_t owner, uint64_t wake_at_us, uint32_t flags)
{
    const uint64_t now = esp_timer_get_time();
    if ((flags & ~XIP_POWER_TRACE_UART) || wake_at_us < now + MIN_SLEEP_MS * 1000 || wake_at_us - now > UINT32_MAX) return XIP_ERR_ARG;
    if (xip_serial_usb_sleep_blocked() || xip_power_active_remaining_ms() || xip_power_awake_for(0)) return XIP_ERR_BUSY;
    if (atomic_exchange(&busy, true)) return XIP_ERR_BUSY;
    request_owner = owner;
    request_activity = atomic_load(&activity_seq);
    request_wifi_generation = current_wifi_generation();
    request_deadline = wake_at_us;
    request_flags = flags;
    atomic_store(&left, 1);
    atomic_store(&state_now, XIP_POWER_AWAKE);
    int rc = xip_power_post();
    if (rc < 0) (void)xip_power_cancel(owner);
    return rc;
}

int xip_power_get_info(xip_power_info *i)
{
    if (!i || i->size < sizeof(*i)) return XIP_ERR_ARG;
    uint32_t size = i->size;
    memset(i, 0, sizeof(*i));
    i->size = size;
    i->state = atomic_load(&state_now);
    i->active_window = xip_power_active_remaining_ms() != 0;
    i->ip_sockets = xip_host_network_sockets();
    i->cycles_left = atomic_load(&left);
    i->wake_cause = atomic_load(&metrics.wake_cause);
    i->soft_sleep = atomic_load(&metrics.soft_now);       /* how the last sleep was done */
    i->sleep_attempts = atomic_load(&metrics.attempts);
    i->sleep_entries = atomic_load(&metrics.entries);
    i->sleep_skipped = atomic_load(&metrics.skipped);
    i->last_requested_us = atomic_load(&metrics.last_requested);
    i->last_duration_us = atomic_load(&metrics.last_duration);
    i->last_resume_us = atomic_load(&metrics.last_resume);
    i->tick_hz = configTICK_RATE_HZ;
    i->tasks = uxTaskGetNumberOfTasks();
    i->ticks = (uint32_t)xTaskGetTickCount();
#if CONFIG_FREERTOS_USE_TICKLESS_IDLE
    i->tickless_idle = 1;
#endif
#if CONFIG_PM_ENABLE
    esp_pm_config_t cfg = {0};
    if (esp_pm_get_configuration(&cfg) == ESP_OK) {
        i->pm_configured = 1;
        i->min_mhz = (uint16_t)cfg.min_freq_mhz;
        i->max_mhz = (uint16_t)cfg.max_freq_mhz;
        i->auto_light_sleep = cfg.light_sleep_enable;
    }
#endif
    i->cpu_mhz = (uint16_t)(esp_clk_cpu_freq() / 1000000);
    const uint32_t caps = MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT;
    i->heap_free = (uint32_t)heap_caps_get_free_size(caps);
    i->heap_min = (uint32_t)heap_caps_get_minimum_free_size(caps);
    i->heap_largest = (uint32_t)heap_caps_get_largest_free_block(caps);
    i->filter_dropped = xip_rx_filter_dropped();
    i->promisc_frames = xip_link_promisc_frames();
    i->promiscuous = xip_link_promisc_on();
    return XIP_OK;
}

/* low-level state to the device log: what a person at the port looks at */
int xip_power_dump(int kind)
{
    switch (kind) {
    case 0:
#if CONFIG_PM_ENABLE
        return esp_pm_dump_locks(stdout) == ESP_OK ? XIP_OK : XIP_ERR_IO;
#else
        return XIP_ERR_UNSUPPORTED;
#endif
    case 1: return xip_heap_dump(2);
    default: return XIP_ERR_ARG;
    }
}
