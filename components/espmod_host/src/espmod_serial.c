/* SPDX-License-Identifier: Apache-2.0 */
/* The serial console as hardware: the host owns the port, a module (or main) gets lines.
 *
 * The driver setup is DMesh's (fw/dmesh-fw-transport/src/uart_esp.rs), kept as it is because it was tuned on boards:
 *   - ESP32-C6: the USB-Serial/JTAG block when a host is attached at boot, else UART0 on GPIO16/17 (the external
 *     bridge of the DevKit). ESP-IDF's "connected" monitor starts out true until it has seen no SOF packets for a few
 *     ticks, so it is asked only after a short delay (asking at once selects an unattached USB endpoint).
 *   - ESP32-S3: UART0 on GPIO43/44, kept through light sleep. ESP32: UART0 on GPIO1/3.
 *   - UART0: 8N1, XTAL clock on S3/C6 and APB on ESP32; the driver owns an event queue and this task is its only
 *     reader (polling uart_read_bytes beside it loses wakeups on some classic bridges). RX-full threshold 64 and a
 *     10-symbol timeout that also fires with an empty FIFO; TX interrupt off.
 *   - FIFO overflow or driver buffer full: flush the input, empty the event queue, drop the half line.
 *   - A UART_DATA event is followed by a zero-wait drain, and so is every other wake: some classic handoffs leave
 *     bytes in the ring without publishing an event.
 *   - USB-Serial/JTAG: buffers sized for two escaped frames per direction, blocking reads.
 * Differences from DMesh, both for stop(): the USB read wakes every 250 ms to look at the stop flag, and a USB write
 * gives up after 200 ms (a closed terminal must not block the task forever).
 *
 * Output is written to the driver directly; ESP_LOG keeps printing on the same port. */
#include <stdarg.h>
#include "esp_log.h"
#include "esp_rom_sys.h"
#include <stdlib.h>
#include <string.h>
#include "sdkconfig.h"
#include "soc/soc_caps.h"
#include "esp_log.h"
#include "esp_sleep.h"
#include "driver/gpio.h"
#include "driver/uart.h"
#if CONFIG_IDF_TARGET_ESP32C6
#include "driver/usb_serial_jtag.h"
#endif
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "espmod_host.h"
#include "internal.h"

static const char *TAG = "serial";

#define LINE_MAX_LEN 160
#define UART_BAUD 115200
#define USB_WAKE_MS 250
#define USB_WRITE_MS 200
#define USB_BUFFER (2 * (2 * (1200 + 1) + 2))     /* two PPP-escaped frames of the transport MTU, as DMesh */

#if CONFIG_IDF_TARGET_ESP32
#define TX_PIN 1
#define RX_PIN 3
#define UART_CLK UART_SCLK_APB
#elif CONFIG_IDF_TARGET_ESP32S3
#define TX_PIN 43
#define RX_PIN 44
#define UART_CLK UART_SCLK_XTAL
#else
#define TX_PIN 16
#define RX_PIN 17
#define UART_CLK UART_SCLK_XTAL
#endif

static struct {
    TaskHandle_t task;
    SemaphoreHandle_t exited;
    QueueHandle_t events;          /* the UART driver's */
    espmod_serial_line_fn on_line;
    void *ctx;
    bool usb;
    volatile bool stop;
    bool running;
} s;

uint64_t em_serial_pins(void) { return (1ull << TX_PIN) | (1ull << RX_PIN); }

/* ---- driver ---- */
static bool install_uart(void)
{
    const uart_config_t cfg = {.baud_rate = UART_BAUD, .data_bits = UART_DATA_8_BITS, .parity = UART_PARITY_DISABLE,
                               .stop_bits = UART_STOP_BITS_1, .flow_ctrl = UART_HW_FLOWCTRL_DISABLE, .source_clk = UART_CLK};
    if (uart_param_config(UART_NUM_0, &cfg) != ESP_OK) return false;
    esp_err_t rc = uart_driver_install(UART_NUM_0, 2048, 0, 16, &s.events, 0);
    if (rc == ESP_ERR_INVALID_STATE) {      /* the IDF console's own driver */
        uart_driver_delete(UART_NUM_0);
        rc = uart_driver_install(UART_NUM_0, 2048, 0, 16, &s.events, 0);
    }
    if (rc != ESP_OK || !s.events) return false;
    uart_set_pin(UART_NUM_0, TX_PIN, RX_PIN, -1, -1);
#if CONFIG_IDF_TARGET_ESP32S3
    gpio_sleep_sel_dis(TX_PIN);
    gpio_sleep_sel_dis(RX_PIN);
#endif
    uart_disable_tx_intr(UART_NUM_0);
    uart_set_rx_full_threshold(UART_NUM_0, 64);
    uart_set_rx_timeout(UART_NUM_0, 10);
    uart_set_always_rx_timeout(UART_NUM_0, true);
    uart_enable_rx_intr(UART_NUM_0);
    uart_set_wakeup_threshold(UART_NUM_0, 3);
    esp_sleep_enable_uart_wakeup(UART_NUM_0);
    return true;
}

static bool install_driver(void)
{
    s.usb = false;
#if CONFIG_IDF_TARGET_ESP32C6
    vTaskDelay(8);
    if (usb_serial_jtag_is_connected()) {
        usb_serial_jtag_driver_config_t cfg = {.tx_buffer_size = USB_BUFFER, .rx_buffer_size = USB_BUFFER};
        esp_err_t rc = usb_serial_jtag_driver_install(&cfg);
        s.usb = rc == ESP_OK || rc == ESP_ERR_INVALID_STATE;
        return s.usb;
    }
#endif
    return install_uart();
}

static void uninstall_driver(void)
{
#if CONFIG_IDF_TARGET_ESP32C6
    if (s.usb) { usb_serial_jtag_driver_uninstall(); s.usb = false; return; }
#endif
    uart_disable_rx_intr(UART_NUM_0);
    uart_driver_delete(UART_NUM_0);
    s.events = NULL;
}

static int read_bytes(uint8_t *buf, size_t n, TickType_t wait)
{
#if CONFIG_IDF_TARGET_ESP32C6
    if (s.usb) return usb_serial_jtag_read_bytes(buf, n, wait);
#endif
    return uart_read_bytes(UART_NUM_0, buf, n, wait);
}

int espmod_serial_write(const void *data, size_t len)
{
    if (!s.running || !len) return s.running ? 0 : ESPMOD_ERR_NOTFOUND;
#if CONFIG_IDF_TARGET_ESP32C6
    if (s.usb) return usb_serial_jtag_write_bytes(data, len, pdMS_TO_TICKS(USB_WRITE_MS));
#endif
    return uart_write_bytes(UART_NUM_0, data, len);
}

/* ---- lines ---- */
typedef struct { char text[LINE_MAX_LEN]; size_t len; } line_t;

/* One byte: CR or LF ends a line (a CRLF pair makes an empty second line, which is not delivered); backspace edits. */
static void feed(line_t *l, uint8_t c)
{
    if (c == '\r' || c == '\n') {
        if (!l->len) return;
        l->text[l->len] = 0;
        size_t n = l->len;
        l->len = 0;
        s.on_line(s.ctx, l->text, n);
    } else if ((c == 0x08 || c == 0x7f) && l->len) {
        l->len--;
    } else if (c >= ' ' && l->len < LINE_MAX_LEN - 1) {
        l->text[l->len++] = (char)c;
    }
}

static void drain(line_t *l)
{
    uint8_t buf[64];
    int n;
    while ((n = read_bytes(buf, sizeof(buf), 0)) > 0)
        for (int i = 0; i < n; i++) feed(l, buf[i]);
}

static void serial_task(void *arg)
{
    (void)arg;
    line_t line = {0};
    uint8_t buf[64];
    while (!s.stop) {
        if (s.usb) {
            int n = read_bytes(buf, sizeof(buf), pdMS_TO_TICKS(USB_WAKE_MS));
            for (int i = 0; i < n; i++) feed(&line, buf[i]);
            continue;
        }
        uart_event_t ev;
        if (xQueueReceive(s.events, &ev, portMAX_DELAY) != pdTRUE) continue;
        if (s.stop) break;
        if (ev.type == UART_FIFO_OVF || ev.type == UART_BUFFER_FULL) {
            uart_flush_input(UART_NUM_0);
            xQueueReset(s.events);
            line.len = 0;
            continue;
        }
        drain(&line);       /* after every wake, not only UART_DATA */
    }
    xSemaphoreGive(s.exited);
    vTaskDelete(NULL);
}

bool espmod_serial_usb_active(void)
{
#if CONFIG_IDF_TARGET_ESP32C6
    return s.running && s.usb && usb_serial_jtag_is_connected();
#else
    return false;
#endif
}

void espmod_serial_rearm(void)
{
    if (!s.running || s.usb) return;
    uart_set_wakeup_threshold(UART_NUM_0, 3);
    uart_enable_rx_intr(UART_NUM_0);
}

int espmod_serial_start(espmod_serial_line_fn on_line, void *ctx)
{
    if (s.running) return ESPMOD_ERR_BUSY;
    if (!on_line) return ESPMOD_ERR_ARG;
    if (!s.exited) s.exited = xSemaphoreCreateBinary();
    if (!s.exited) return ESPMOD_ERR_NOMEM;
    s.on_line = on_line;
    s.ctx = ctx;
    s.stop = false;
    if (!install_driver()) { ESP_LOGE(TAG, "no serial driver"); return ESPMOD_ERR_IO; }
    s.running = true;
    if (xTaskCreate(serial_task, "em_serial", CONFIG_ESPMOD_SERIAL_STACK, NULL, 4, &s.task) != pdPASS) {
        s.running = false;
        uninstall_driver();
        return ESPMOD_ERR_NOMEM;
    }
    ESP_LOGW(TAG, "lines on %s", s.usb ? "USB-Serial/JTAG" : "UART0");
    return 0;
}

void espmod_serial_stop(void)
{
    if (!s.running) return;
    s.stop = true;
    if (!s.usb && s.events) {       /* wake the reader with an event of its own */
        uart_event_t ev = {.type = UART_EVENT_MAX};
        xQueueSend(s.events, &ev, 0);
    }
    /* A line handler that never returns holds the task; then the driver is left alone rather than pulled from under it. */
    if (xSemaphoreTake(s.exited, pdMS_TO_TICKS(2000)) != pdTRUE) { ESP_LOGE(TAG, "reader did not stop"); return; }
    uninstall_driver();
    s.running = false;
}

static int rom_vprintf(const char *fmt, va_list ap)
{
    char line[160];
    int n = vsnprintf(line, sizeof(line), fmt, ap);
    esp_rom_printf("%s", line);
    return n;
}

void espmod_log_to_rom(void) { esp_log_set_vprintf(rom_vprintf); }
