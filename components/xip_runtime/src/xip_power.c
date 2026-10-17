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
 *   which sees the cause in HOST_WAKE, decides what follows (DMesh: stay awake for a 60 s grace and accept the profile change).
 *   Between two cycles of one request there is an awake time (a timer that queues the next cycle) so modules run.
 *
 * USB-SERIAL/JTAG: WHY A SLEEP IS SOFT WHEN A HOST IS ATTACHED  (settled after bench tests, 2026-10-17; do not re-open without new evidence)
 *   The C6 can take its console on the native USB-Serial/JTAG port. Light sleep powers that port down: ESP-IDF disables its pad and clock
 *   (esp_hw_support/sleep_modes.c; SOC_USB_SERIAL_JTAG_SUPPORT_LIGHT_SLEEP is commented out for the C6 in soc_caps.h, "TODO IDF-6395"; DMesh does not define it either).
 *   The host therefore sees a disconnect on every physical sleep. On the bench (three C6 boards, Linux host) this left the device in a state that only a replug
 *   or a reset cleared: 500 ms sleeps with 50 ms or 1 s awake left it half-enumerated (a USB device with no strings and no ttyACM) or with a silent console, and
 *   even 4 s sleeps with 3 s awake made one board vanish from the bus after working once. DMesh knows this too: its comment in main_runtime.rs says USB-JTAG
 *   "is powered down by physical light sleep" and it keeps the CPU awake there (usb_jtag_debug_hold, the `sleepy-soft` boot policy).
 *   So, as DMesh: while the console is on USB-Serial/JTAG AND a host is attached (xip_serial_usb_active), the sleep is SOFT - the same radio steps 1 and 5 run
 *   around a plain delay and the CPU does not sleep - and info.soft_sleep says how the last sleep was done. The soft sleep still proves the radio/state machine.
 *   Why this is acceptable: a device on USB is being charged by the host (laptop or phone), so it may use a bit more power and the saving is not the point there.
 *   The way out in the field is to DISCONNECT USB cleanly when the device is not in use, so that sleep can start again; with USB away the sleep is physical (the same
 *   code, untested on the bench for lack of a way to unplug remotely), and the sleep-related information (SLEEP_PENDING / WAKE) is carried by the DW announces
 *   (NAN publish, ESP-NOW broadcast) instead of the port. The C6 boards with two connectors were switched to the slower UART console for this reason; a UART
 *   console sleeps physically (tested: 13 cycles of 500 ms, radio back in ~10 ms, early wake by a byte on the UART).
 *   There is deliberately no option to force a physical sleep on USB. If someone needs to look at it again, the experiment is: build with the soft test removed,
 *   use a board you can replug, and expect to lose the USB port.
 *
 * Runs on the host worker. The Rust twin of the radio steps is crates/xip-runtime-esp/src/wifi.rs (power_suspend / power_resume); this file is shared by both runtimes. */
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
#include "internal.h"

#define MIN_SLEEP_MS 10
#define DEFAULT_AWAKE_MS 50
/* DMesh's active policy: 80..160 MHz, light sleep only when asked explicitly */
#define PM_MIN_MHZ 80
#define PM_MAX_MHZ 160

uint32_t esp_clk_cpu_freq(void);
static xip_power_radio radio;
static atomic_bool busy;
static atomic_uchar state_now;
static atomic_ushort left;
static atomic_uint duration_ms, awake_ms;
static atomic_uint attempts, entries, skipped, last_requested, last_duration, last_resume;
static atomic_uchar wake_cause, soft_now;
static esp_timer_handle_t awake_timer;
static bool pm_done;

void xip_power_set_radio(const xip_power_radio *r) { radio = r ? *r : (xip_power_radio){0}; }

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
    xip_power_post();
}

/* One cycle, on the worker. */
void xip_power_cycle(void)
{
    if (!atomic_load(&left)) { atomic_store(&state_now, XIP_POWER_IDLE); atomic_store(&busy, false); return; }
    configure_pm();
    const uint64_t asked_us = (uint64_t)atomic_load(&duration_ms) * 1000;
    const bool had_radio = radio.suspend && radio.suspend();
    atomic_store(&state_now, XIP_POWER_SLEEPING);
    atomic_fetch_add(&attempts, 1);
    atomic_store(&last_requested, (uint32_t)asked_us);
    /* Soft sleep while a host is attached to the USB-Serial/JTAG console: a physical sleep disconnects the port and the host does not recover it
     * (see the header: the bench results, DMesh's usb_jtag_debug_hold, and why this is accepted). UART console, or no host attached: physical. */
    const bool soft = xip_serial_usb_active();
    atomic_store(&soft_now, soft);
    esp_err_t rc = ESP_OK;
    int64_t before = esp_timer_get_time(), after = before;
    if (soft) {
        vTaskDelay(pdMS_TO_TICKS(atomic_load(&duration_ms)));
        after = esp_timer_get_time();
    } else {
        rc = esp_sleep_enable_timer_wakeup(asked_us);
        if (rc == ESP_OK) {
            rc = esp_light_sleep_start();
            after = esp_timer_get_time();
        }
    }
    atomic_store(&last_duration, (uint32_t)(after - before));
    if (!soft) {   /* a bit mask of causes (1 << esp_sleep_wakeup_cause_t); report the lowest */
        const uint32_t causes = esp_sleep_get_wakeup_causes();
        uint8_t c = 0;
        for (uint8_t b = 1; b < 32 && !c; b++) if (causes & (1u << b)) c = b;
        atomic_store(&wake_cause, c);
    }
    if (rc == ESP_OK) atomic_fetch_add(&entries, 1);
    else atomic_fetch_add(&skipped, 1);
    if (!soft) rearm_uart();
    atomic_store(&state_now, XIP_POWER_RESUMING);
    const int64_t resume_start = esp_timer_get_time();
    if (had_radio && radio.resume) radio.resume();
    atomic_store(&last_resume, (uint32_t)(esp_timer_get_time() - resume_start));
    if (rc != ESP_OK) {              /* not a wake: restored once, and no more cycles */
        atomic_store(&left, 0);
        atomic_store(&state_now, XIP_POWER_IDLE);
        atomic_store(&busy, false);
        return;
    }
    atomic_fetch_sub(&left, 1);
    /* A wake that is not the timer (a person on the UART, a GPIO) swaps the device to active, as DMesh: the rest of the request is dropped and the module,
     * which gets the cause as the value of HOST_WAKE, decides what follows. */
    const uint8_t cause = soft ? 4 : atomic_load(&wake_cause);
    if (cause != 4) atomic_store(&left, 0);
    xip_host_wake_window_value(cause);   /* HOST_WAKE to the modules with a window alarm */
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
}

int xip_power_request(const xip_power_sleep *req)
{
    if (!req || req->size < sizeof(*req) || req->duration_ms < MIN_SLEEP_MS) return XIP_ERR_ARG;
    if (atomic_exchange(&busy, true)) return XIP_ERR_BUSY;
    atomic_store(&duration_ms, req->duration_ms);
    atomic_store(&awake_ms, req->awake_ms ? req->awake_ms : DEFAULT_AWAKE_MS);
    atomic_store(&left, req->cycles ? req->cycles : 1);
    atomic_store(&state_now, XIP_POWER_AWAKE);
    xip_power_post();
    return XIP_OK;
}

int xip_power_get_info(xip_power_info *i)
{
    if (!i || i->size < sizeof(*i)) return XIP_ERR_ARG;
    uint32_t size = i->size;
    memset(i, 0, sizeof(*i));
    i->size = size;
    i->state = atomic_load(&state_now);
    i->cycles_left = atomic_load(&left);
    i->wake_cause = atomic_load(&wake_cause);
        i->soft_sleep = atomic_load(&soft_now);       /* how the last sleep was done */
    i->sleep_attempts = atomic_load(&attempts);
    i->sleep_entries = atomic_load(&entries);
    i->sleep_skipped = atomic_load(&skipped);
    i->last_requested_us = atomic_load(&last_requested);
    i->last_duration_us = atomic_load(&last_duration);
    i->last_resume_us = atomic_load(&last_resume);
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
