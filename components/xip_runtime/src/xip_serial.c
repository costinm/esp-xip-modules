/* SPDX-License-Identifier: Apache-2.0 */
/* The serial line as hardware: the runtime owns the port and its codec turns the bytes into records for module ports.
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
 * Bounded waits for stop(): the USB read wakes every 250 ms to look at the stop flag, and a USB write
 * gives up after 200 ms (a closed terminal must not block the task forever).
 *
 * A product client retains its own codec/pool and uses the independent writer
 * callback; the physical driver, tasks and teardown are shared. */
#include <stdarg.h>
#include <stdatomic.h>
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
#if SOC_USB_SERIAL_JTAG_SUPPORTED
#include "driver/usb_serial_jtag.h"
#include "hal/usb_serial_jtag_ll.h"
#endif
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "xip_runtime.h"
#include "xip_wifi.h"
#include "internal.h"

static const char *TAG = "serial";
#if SOC_USB_SERIAL_JTAG_SUPPORTED
static atomic_bool usb_detached;
#endif

#define UART_BAUD 115200
#define USB_WAKE_MS 250
#define USB_WRITE_MS 200
#define USB_BUFFER (2 * (2 * (XIP_PKT_MAX + 1) + 2))     /* two PPP-escaped frames of the transport MTU, as DMesh */

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
    TaskHandle_t tx_task;
    xip_serial_client_t client;
    xip_serial_packet_client packets;
    xip_serial_packet_tx sending;
    atomic_uint tx_state; /* 0 free, 1 taking ownership, 2 accepted, 3 sending */
    atomic_bool rx_enabled;
    int baud;
    size_t usb_buffer_size;
    bool opened;
    unsigned joined;
    SemaphoreHandle_t exited;
    SemaphoreHandle_t frame_lock;
    atomic_uint pending_baud;
    atomic_uint activity_ms;
    QueueHandle_t events;          /* the UART driver's */
    bool usb;
    volatile bool stop;
    bool running;
} s = {.activity_ms = ATOMIC_VAR_INIT(5000), .rx_enabled = ATOMIC_VAR_INIT(true)};

uint64_t xip_serial_pins(void) { return (1ull << TX_PIN) | (1ull << RX_PIN); }

/* ---- driver ---- */
static bool install_uart(void)
{
    const uart_config_t cfg = {.baud_rate = s.baud, .data_bits = UART_DATA_8_BITS, .parity = UART_PARITY_DISABLE,
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
        usb_serial_jtag_driver_config_t cfg = {.tx_buffer_size = s.usb_buffer_size, .rx_buffer_size = s.usb_buffer_size};
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

int xip_serial_write(const void *data, size_t len)
{
    if (!s.running || s.stop || !len) return s.running && !s.stop ? 0 : XIP_ERR_NOTFOUND;
#if CONFIG_IDF_TARGET_ESP32C6
    if (s.usb) {
        /* An explicitly detached PHY has no receiver. Reports must not fill
         * the USB driver's queue or hold a DW open waiting for a host. */
        if (atomic_load(&usb_detached)) return XIP_ERR_NOTFOUND;
        return usb_serial_jtag_write_bytes(data, len, pdMS_TO_TICKS(USB_WRITE_MS));
    }
#endif
    return uart_write_bytes(UART_NUM_0, data, len);
}

static void apply_pending_baud(void);
static void packet_event(uint32_t event, uint32_t value);
static void logs_to_frames(void);
static void logs_to_console(void);

static const xip_serial_codec_t *codec;
void xip_serial_set_codec(const xip_serial_codec_t *c) { codec = c; }

/* A chunk the driver read: the framing codec installed at start (the PPP codec of
 * xip_ppp.c: records to their port) has it all. */
static void chunk(const uint8_t *buf, int n)
{
    /* Local UART/USB is a trusted wake source: the first byte must keep the
     * next DW awake long enough to receive the rest of the packet. Network
     * traffic gets its activity vote only after decrypted CBOR admission. */
    xip_serial_note_wake();
    if (!s.usb && !atomic_load(&s.rx_enabled)) return;
    if (s.client.on_bytes) s.client.on_bytes(s.client.ctx, buf, (size_t)n);
    else if (codec && codec->on_bytes) codec->on_bytes(buf, (size_t)n);
}

/* True while a writer owns the gap-free record lane (write_frame's codec TX or
 * a control record). The log lane must never splice a line into one of them. */
static _Atomic bool frame_open;

static int write_frame(uint32_t port, const void *data, size_t len)
{
    if (!s.frame_lock || xSemaphoreTake(s.frame_lock, portMAX_DELAY) != pdTRUE) return XIP_ERR_BUSY;
    atomic_store(&frame_open, true);
    int rc = codec && codec->tx ? codec->tx(port, data, len) : xip_serial_write(data, len);
    atomic_store(&frame_open, false);
    xSemaphoreGive(s.frame_lock);
    if (!atomic_load(&s.tx_state)) packet_event(7, 1);
    return rc;
}

/* Native module PPP is synchronous. A packet client must use the one
 * ownership-bearing admission slot; no caller may bypass it with a borrowed
 * frame while an accepted packet is waiting/sending. */
int xip_serial_write_frame(uint32_t port, const void *data, size_t len)
{
    if (s.client.on_bytes) return XIP_ERR_UNSUPPORTED;
    return write_frame(port, data, len);
}

/* One pool writer per received frame, from the module pool or an application's
 * pool vtable. The PPP codec owns it until commit/abort; no payload staging. */
static void packet_event(uint32_t event, uint32_t value)
{
    if (s.packets.event) s.packets.event(s.packets.ctx, event, value);
}
int xip_serial_frame_begin(uint32_t port, xip_link_rx_buf *buf)
{
    if (!s.packets.rx_begin) return xip_serial_rx_begin(port, buf);
    xip_serial_rx_buffer out = {0};
    int rc = s.packets.rx_begin(s.packets.ctx, port, &out);
    if (rc < 0) packet_event(6, 1);
    if (rc) return rc;
    if (!out.data || !out.capacity) {
        s.packets.rx_finish(s.packets.ctx, port, 0, XIP_ERR_ARG);
        return XIP_ERR_ARG;
    }
    *buf = (xip_link_rx_buf){.data = out.data, .capacity = out.capacity, .slot = -1};
    return XIP_OK;
}
void xip_serial_frame_finish(uint32_t port, const xip_link_rx_buf *buf, size_t len, int outcome)
{
    if (s.packets.rx_finish) {
        s.packets.rx_finish(s.packets.ctx, port, len, outcome);
        if (!outcome) packet_event(4, 1);
    } else if (outcome) xip_serial_rx_abort(buf);
    else xip_serial_rx_commit(port, buf, len);
}

void xip_serial_receive_enabled(bool enabled) { atomic_store(&s.rx_enabled, enabled); }
static void packet_bytes(void *ctx, const uint8_t *data, size_t len)
{
    (void)ctx;
    packet_event(1, len);
    if (!s.usb && !atomic_load(&s.rx_enabled)) return;
    codec->on_bytes(data, len);
    taskYIELD();
}
static void packet_reset(void *ctx)
{
    (void)ctx;
    if (codec && codec->on_reset) codec->on_reset();
    packet_event(3, 1);
}
static void packet_uart_event(void *ctx) { (void)ctx; packet_event(2, 1); }

static void packet_tx(void *ctx)
{
    (void)ctx;
    unsigned expected = 2;
    if (!atomic_compare_exchange_strong(&s.tx_state, &expected, 3)) return;
    xip_serial_packet_tx packet = s.sending;
    int rc = write_frame(packet.port, packet.data, packet.len);
    /* Complete while admission is closed: the callback destroys the typed
     * token. Only then publish free and wake a sender that observed busy. */
    packet.complete(packet.owner, rc);
    memset(&s.sending, 0, sizeof(s.sending));
    atomic_store(&s.tx_state, 0);
    packet_event(7, 1);
}

bool xip_serial_tx_available(void) { return s.running && !s.stop && !atomic_load(&s.tx_state); }

int xip_serial_submit_packet(void *source, void (*take)(void *, void *, xip_serial_packet_tx *))
{
    if (!take || !s.frame_lock) return XIP_ERR_ARG;
    if (atomic_load(&s.tx_state) || xSemaphoreTake(s.frame_lock, 0) != pdTRUE) return XIP_ERR_BUSY;
    int rc = XIP_ERR_BUSY;
    if (!s.running || s.stop || !s.packets.rx_begin) rc = XIP_ERR_NOTFOUND;
    else if (!atomic_load(&s.tx_state)) {
        atomic_store(&s.tx_state, 1);
        /* Synchronous move from caller-owned source into the one current
         * frame. A busy caller keeps its exact original packet. */
        take(s.packets.ctx, source, &s.sending);
        atomic_store(&s.tx_state, 2);
        rc = XIP_OK;
    }
    xSemaphoreGive(s.frame_lock);
    if (!rc) xip_serial_tx_ready();
    return rc;
}

static void packet_close(void *ctx)
{
    (void)ctx;
    if (codec && codec->on_reset) codec->on_reset(); /* return partial RX writer */
    xip_serial_packet_client client = s.packets;
    memset(&s.packets, 0, sizeof(s.packets));
    if (client.close) client.close(client.ctx);
}

int xip_serial_start_packets(const xip_serial_packet_client *client)
{
    if (!client) return XIP_ERR_ARG;
    if (!s.opened || s.running || !client->rx_begin || !client->rx_finish) {
        if (client->close) client->close(client->ctx);
        return XIP_ERR_BUSY;
    }
    extern const xip_serial_codec_t xip_ppp_codec;
    xip_serial_set_codec(&xip_ppp_codec);
    s.packets = *client;
    xip_serial_client_t native = {.on_bytes = packet_bytes, .on_reset = packet_reset,
        .on_tx = packet_tx, .on_uart_event = packet_uart_event, .on_close = packet_close};
    int rc = xip_serial_start_client(&native);
    if (!rc) logs_to_frames();
    return rc;
}

static void drain(void)
{
    uint8_t buf[256];
    int n;
    while ((n = read_bytes(buf, sizeof(buf), 0)) > 0)
        chunk(buf, n);
}

static void serial_task(void *arg)
{
    (void)arg;
    uint8_t buf[256];
    while (!s.stop) {
        if (s.usb) {
            int n = read_bytes(buf, sizeof(buf), pdMS_TO_TICKS(USB_WAKE_MS));
            if (n > 0) chunk(buf, n);
            continue;
        }
        uart_event_t ev;
        if (xQueueReceive(s.events, &ev, portMAX_DELAY) != pdTRUE) continue;
        if (s.stop) break;
        if (s.client.on_uart_event) s.client.on_uart_event(s.client.ctx);
        if (ev.type == UART_FIFO_OVF || ev.type == UART_BUFFER_FULL) {
            uart_flush_input(UART_NUM_0);
            xQueueReset(s.events);
            if (s.client.on_reset) s.client.on_reset(s.client.ctx);
            else if (codec && codec->on_reset) codec->on_reset();
            continue;
        }
        drain();       /* after every wake, not only UART_DATA */
    }
    xSemaphoreGive(s.exited);
    vTaskDelete(NULL);
}

/* The port is USB-Serial/JTAG (selected at boot), connected or not: light sleep powers it down. */
bool xip_serial_usb_selected(void) { return s.usb; }

/* SOF attachment is independent of which serial driver boot selected. IDF has
 * no connection callback; the host worker samples this state every 250 ms. */
bool xip_serial_usb_active(void)
{
#if SOC_USB_SERIAL_JTAG_SUPPORTED
    return usb_serial_jtag_is_connected();
#else
    return false;
#endif
}

/* Experimental opt-in; the default never detaches an attached host. Read at
 * driver open so settings changes are explicit at the next boot/reopen. */
static atomic_bool usb_sleep_allowed, usb_requested_on;
#if SOC_USB_SERIAL_JTAG_SUPPORTED
static bool saved_pull_override;
static usb_serial_jtag_pull_override_vals_t saved_pulls;
#endif
bool xip_serial_usb_sleep_allowed(void) { return atomic_load(&usb_sleep_allowed) && !atomic_load(&usb_requested_on); }
bool xip_serial_usb_sleep_blocked(void) { return xip_serial_usb_active() && !xip_serial_usb_sleep_allowed(); }
void xip_serial_sleep_suspend(void)
{
#if SOC_USB_SERIAL_JTAG_SUPPORTED
    if (!xip_serial_usb_sleep_allowed() || usb_detached) return;
    /* IDF 6 has no hal/usb_phy_ll.h / usb_phy_ll_usb_wrap_pad_enable_pullup.
     * USJ owns this PHY on C6/S3; removing its D+ pull-up simulates unplug. */
    saved_pulls = (usb_serial_jtag_pull_override_vals_t){
        .dp_pu=USB_SERIAL_JTAG.conf0.dp_pullup, .dp_pd=USB_SERIAL_JTAG.conf0.dp_pulldown,
        .dm_pu=USB_SERIAL_JTAG.conf0.dm_pullup, .dm_pd=USB_SERIAL_JTAG.conf0.dm_pulldown};
    saved_pull_override=USB_SERIAL_JTAG.conf0.pad_pull_override;
    const usb_serial_jtag_pull_override_vals_t detached={0};
    usb_serial_jtag_ll_phy_enable_pull_override(&detached);
    usb_detached=true;
    /* Give the host and IDF's SOF/PM-lock monitor time to observe disconnect.
     * The power owner recomputes its absolute wake deadline afterwards. */
    vTaskDelay(pdMS_TO_TICKS(20));
#endif
}
static void serial_usb_on(void)
{
#if SOC_USB_SERIAL_JTAG_SUPPORTED
    if (!usb_detached) return;
    usb_serial_jtag_ll_phy_enable_pull_override(&saved_pulls);
    if (!saved_pull_override) usb_serial_jtag_ll_phy_disable_pull_override();
    usb_detached=false;
#endif
}
static int serial_usb_sleep(void *ctx) { (void)ctx; return xip_serial_usb_sleep_allowed(); }
static int serial_usb(void *ctx) { (void)ctx; return xip_serial_usb_active(); }
static int serial_mode(void *ctx, uint32_t mode)
{
    (void)ctx;
    if (mode > 1) return XIP_ERR_ARG;
    atomic_store(&usb_requested_on,mode!=0);
    if (mode) serial_usb_on(); else xip_serial_sleep_suspend();
    /* Leave the UART wake interrupt armed even when application RX is off.
     * A deliberately detached USB PHY is restored only by serial-on policy. */
    xip_serial_receive_enabled(mode != 0);
    xip_serial_activity_window(5000);
    return XIP_OK;
}
static int serial_baud(void *ctx, uint32_t baud) { (void)ctx; return s.usb ? XIP_OK : xip_serial_request_baud(baud); }
static const xip_serial serial_component = {.version=1, .size=sizeof(xip_serial),
    .usb_connected=serial_usb, .mode=serial_mode, .baud=serial_baud, .usb_sleep_allowed=serial_usb_sleep};
const xip_serial *xip_serial_component(void) { return &serial_component; }

void xip_serial_rearm(void)
{
    if (!s.running || s.usb) return;
    uart_set_wakeup_threshold(UART_NUM_0, 3);
    uart_enable_rx_intr(UART_NUM_0);
}

/* The writer drains an ownership-bearing client slot, never a private C
 * packet queue. Separate tasks preserve full duplex when the host stops RX. */
static void serial_tx_task(void *arg)
{
    (void)arg;
    while (!s.stop) {
        s.client.on_tx(s.client.ctx);
        apply_pending_baud();
        ulTaskNotifyTake(pdTRUE, portMAX_DELAY);
    }
    xSemaphoreGive(s.exited);
    vTaskDelete(NULL);
}

void xip_serial_tx_ready(void) { if (s.tx_task) xTaskNotifyGive(s.tx_task); }

int xip_serial_drain(void)
{
    if (!s.running || s.stop) return XIP_ERR_NOTFOUND;
    if (atomic_load(&s.tx_state)) return XIP_ERR_BUSY;
#if CONFIG_IDF_TARGET_ESP32C6
    if (s.usb) {
        if (atomic_load(&usb_detached) || !usb_serial_jtag_is_connected()) return XIP_OK;
        return usb_serial_jtag_wait_tx_done(pdMS_TO_TICKS(100))==ESP_OK ? XIP_OK : XIP_ERR_IO;
    }
#endif
    return uart_wait_tx_done(UART_NUM_0, pdMS_TO_TICKS(100)) == ESP_OK ? XIP_OK : XIP_ERR_IO;
}

int xip_serial_buffered_bytes(size_t *bytes)
{
    if (!s.opened || s.usb || !bytes) return XIP_ERR_NOTFOUND;
    return uart_get_buffered_data_len(UART_NUM_0, bytes) == ESP_OK ? XIP_OK : XIP_ERR_IO;
}

int xip_serial_set_baud(uint32_t baud)
{
    if (!s.opened || s.usb) return XIP_ERR_NOTFOUND;
    if (uart_wait_tx_done(UART_NUM_0, pdMS_TO_TICKS(100)) != ESP_OK) return XIP_ERR_IO;
    if (uart_set_baudrate(UART_NUM_0, baud) != ESP_OK) return XIP_ERR_IO;
    s.baud = (int)baud;
    return XIP_OK;
}

/* Control records carry no packet: one rate plus an ACK bit is the entire
 * pending state. Only the TX owner emits ACK and changes the physical rate. */
#define BAUD_ACK (1u << 31)
static bool baud_valid(uint32_t baud)
{
    return baud == 115200 || baud == 230400 || baud == 460800 || baud == 921600;
}

static void apply_pending_baud(void)
{
    unsigned pending = atomic_exchange(&s.pending_baud, 0);
    if (!pending || s.stop) return;
    if (xSemaphoreTake(s.frame_lock, portMAX_DELAY) != pdTRUE) return;
    uint32_t baud = pending & ~BAUD_ACK;
    bool sent = true;
    if (pending & BAUD_ACK) {
        const uint8_t record[] = {0xf9, 1, baud >> 24, baud >> 16, baud >> 8, baud};
        uint8_t wire[14];
        size_t n = 0;
        wire[n++] = 0x7e;
        for (size_t i = 0; i < sizeof(record); ++i) {
            if (record[i] == 0x7e || record[i] == 0x7d) { wire[n++] = 0x7d; wire[n++] = record[i] ^ 0x20; }
            else wire[n++] = record[i];
        }
        wire[n++] = 0x7e;
        atomic_store(&frame_open, true);
        for (size_t at = 0; at < n;) {
            int w = xip_serial_write(wire + at, n - at);
            if (w <= 0) { sent = false; break; }
            at += (size_t)w;
        }
        atomic_store(&frame_open, false);
    }
    /* uart_wait_tx_done in set_baud drains the old-rate ACK before switching.
     * A failed write/drain never changes the rate. USB has no baud exchange. */
    if (sent) (void)xip_serial_set_baud(baud);
    xSemaphoreGive(s.frame_lock);
    if (!atomic_load(&s.tx_state)) packet_event(7, 1);
}

static int queue_baud(uint32_t baud, bool ack)
{
    if (!baud_valid(baud)) return XIP_ERR_ARG;
    if (!s.running || s.stop || s.usb) return XIP_ERR_UNSUPPORTED;
    unsigned expected = 0;
    if (!atomic_compare_exchange_strong(&s.pending_baud, &expected, baud | (ack ? BAUD_ACK : 0))) return XIP_ERR_BUSY;
    if (s.tx_task) xip_serial_tx_ready();
    else apply_pending_baud(); /* generic PPP RX; frame_lock serializes module TX */
    return XIP_OK;
}

int xip_serial_request_baud(uint32_t baud) { return queue_baud(baud, false); }

int xip_serial_control(uint32_t port, const uint8_t *data, size_t len)
{
    if (port == 0xf8) {
        if (len) return XIP_ERR_ARG;
        xip_power_activate(5000); /* trusted local serial wake */
        packet_event(5, 1);
        return XIP_OK;
    }
    if (port != 0xf9) return XIP_ERR_UNSUPPORTED;
    if (!data || len != 5 || data[0] != 0) return XIP_ERR_ARG;
    uint32_t baud = ((uint32_t)data[1] << 24) | ((uint32_t)data[2] << 16) |
                    ((uint32_t)data[3] << 8) | data[4];
    return queue_baud(baud, true);
}

void xip_serial_activate(void) { xip_power_activate(atomic_load(&s.activity_ms)); }

void xip_serial_activity_window(uint32_t ms) { atomic_store(&s.activity_ms, ms); }

int xip_serial_open(int baud, size_t usb_buffer_size)
{
    if (baud <= 0 || usb_buffer_size < 256 || usb_buffer_size > 65536) return XIP_ERR_ARG;
    xip_serial_stop();
    if (s.running) return XIP_ERR_BUSY;
    s.frame_lock = xSemaphoreCreateMutex();
    if (!s.frame_lock) return XIP_ERR_NOMEM;
    atomic_store(&s.pending_baud, 0);
    s.baud = baud;
    s.usb_buffer_size = usb_buffer_size;
    uint8_t setting[4]={0};
    atomic_store(&usb_sleep_allowed,xip_wifi_nv("usb.sleep",9,NULL,0,setting,sizeof(setting))==1 && setting[0]=='1');
    if (!install_driver()) { vSemaphoreDelete(s.frame_lock); s.frame_lock = NULL; return XIP_ERR_IO; }
    s.opened = true;
    return XIP_OK;
}

static int start_tasks(void)
{
    if (!s.exited) s.exited = xSemaphoreCreateCounting(2, 0);
    if (!s.exited) goto failed;
    s.stop = false;
    s.joined = 0;
    s.running = true;
    if (s.client.on_tx && xTaskCreate(serial_tx_task, "xip_serial_tx", CONFIG_XIP_SERIAL_STACK,
                                     NULL, 5, &s.tx_task) != pdPASS) goto failed;
    unsigned priority = 4;
    if (s.client.on_bytes) {
#if CONFIG_IDF_TARGET_ESP32C6
        priority = 5; /* DMesh USB/fallback RX and TX share priority. */
#else
        priority = 6; /* Physical UART RX retains its event-owner priority. */
#endif
    }
    if (xTaskCreate(serial_task, "xip_serial", CONFIG_XIP_SERIAL_STACK, NULL,
                    priority, &s.task) != pdPASS) goto failed;
    return XIP_OK;
failed:
    xip_serial_stop();
    return XIP_ERR_NOMEM;
}

int xip_serial_start_client(const xip_serial_client_t *client)
{
    if (!client) return XIP_ERR_ARG;
    if (!s.opened || s.running || !client->on_bytes) {
        if (client->on_close) client->on_close(client->ctx);
        return XIP_ERR_BUSY;
    }
    s.client = *client;
    return start_tasks();
}

int xip_serial_start(void)
{
    if (s.running) return XIP_ERR_BUSY;
    int rc = xip_serial_open(UART_BAUD, USB_BUFFER);
    if (rc) return rc;
    extern const xip_serial_codec_t xip_ppp_codec;
    xip_serial_set_codec(&xip_ppp_codec);
    rc = start_tasks();
    if (!rc) logs_to_frames();
    if (!rc) ESP_LOGW(TAG, "records on %s (the PPP codec)", s.usb ? "USB-Serial/JTAG" : "UART0");
    return rc;
}

void xip_serial_stop(void)
{
    s.stop = true;
    /* Wait for any synchronous ownership transfer before destroying context. */
    if (s.frame_lock && xSemaphoreTake(s.frame_lock, portMAX_DELAY) == pdTRUE) xSemaphoreGive(s.frame_lock);
    if (s.running) {
        if (!s.usb && s.events) {
            uart_event_t ev = {.type = UART_EVENT_MAX};
            xQueueSend(s.events, &ev, 0);
        }
        xip_serial_tx_ready();
        unsigned tasks = (s.task != NULL) + (s.tx_task != NULL);
        while (s.joined < tasks) {
            if (xSemaphoreTake(s.exited, pdMS_TO_TICKS(2000)) != pdTRUE) {
                ESP_LOGE(TAG, "serial task did not stop");
                return; /* retain driver and client while callbacks can run */
            }
            ++s.joined;
        }
    }
    s.task = NULL;
    s.tx_task = NULL;
    s.running = false;
    if (atomic_exchange(&s.tx_state, 0) == 2) {
        s.sending.complete(s.sending.owner, XIP_ERR_IO);
        memset(&s.sending, 0, sizeof(s.sending));
    }
    if (s.opened) uninstall_driver();
    s.opened = false;
    atomic_store(&s.pending_baud, 0);
    logs_to_console(); /* before the mutex goes away: no hook may outlive it */
    if (s.frame_lock) { vSemaphoreDelete(s.frame_lock); s.frame_lock = NULL; }
    if (s.client.on_close) s.client.on_close(s.client.ctx);
    else if (codec && codec->on_reset) codec->on_reset();
    memset(&s.client, 0, sizeof(s.client));
    if (s.exited) { vSemaphoreDelete(s.exited); s.exited = NULL; }
}

/* The log lane shares the wire with PPP records. The host reader treats raw
 * bytes between records as text and discards bytes it cannot frame, but a
 * line spliced into a record corrupts both. So each line goes out under the
 * same frame_lock that serializes whole records: a line lands atomically in
 * the gap between records. Output is never queued: while a record is open,
 * from an ISR, or when the lock cannot be taken immediately, the line is
 * dropped and counted. Busy log output must not hold the CPU, a DW or the
 * record lane. */
static int (*log_prev)(const char *, va_list);
static bool log_installed;
static _Atomic uint32_t log_dropped;

static int log_vprintf(const char *fmt, va_list ap)
{
    if (!s.running || s.stop) return log_prev ? log_prev(fmt, ap) : 0;
    char line[256];
    int n = vsnprintf(line, sizeof(line), fmt, ap);
    if (n < 0) return n;
    if (atomic_load(&frame_open) || xPortInIsrContext() ||
        xSemaphoreTake(s.frame_lock, 0) != pdTRUE) {
        atomic_fetch_add(&log_dropped, 1u);
        return n;
    }
    size_t left = (size_t)(n < (int)sizeof(line) ? n : (int)sizeof(line) - 1);
    for (size_t at = 0; at < left;) {
        int w = xip_serial_write(line + at, left - at);
        if (w <= 0) break;
        at += (size_t)w;
    }
    xSemaphoreGive(s.frame_lock);
    return n;
}

static void logs_to_frames(void)
{
    if (log_installed) return;
    log_prev = esp_log_set_vprintf(log_vprintf);
    log_installed = true;
}

static void logs_to_console(void)
{
    if (!log_installed) return;
    (void)esp_log_set_vprintf(log_prev);
    log_prev = NULL;
    log_installed = false;
}

static int rom_vprintf(const char *fmt, va_list ap)
{
    char line[160];
    int n = vsnprintf(line, sizeof(line), fmt, ap);
    esp_rom_printf("%s", line);
    return n;
}

void xip_log_to_rom(void)
{
    /* Once the PPP codec owns the port the framed lane must stay installed;
     * before that the ROM console is the caller's own choice, as before. */
    if (!log_installed) (void)esp_log_set_vprintf(rom_vprintf);
}
