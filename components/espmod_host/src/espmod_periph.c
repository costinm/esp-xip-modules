/* SPDX-License-Identifier: Apache-2.0 */
/* Peripheral calls of the host table. The rules that matter for a low-trust module:
 *  - a pin has one owner; a second module (or a second claim of a reserved pin) gets ESPMOD_ERR_BUSY;
 *  - every wait is bounded (SPI, ready line, I2C, ADC);
 *  - interrupts only queue an event; they never call the module;
 *  - espmod_instance_destroy() undoes all of it (em_periph_release). */
#include <stdlib.h>
#include <string.h>
#include "driver/gpio.h"
#include "driver/i2c_master.h"
#include "driver/rmt_tx.h"
#include "driver/spi_master.h"
#include "esp_adc/adc_oneshot.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_rom_sys.h"
#include "hal/gpio_ll.h"
#include "soc/gpio_struct.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "internal.h"

static const char *TAG = "espmod";
#define PINS 64
static const void *pin_owner[PINS];
static portMUX_TYPE owner_lock = portMUX_INITIALIZER_UNLOCKED;
static bool isr_service;
static uint64_t reserved_pins;

void em_periph_reserve(uint64_t mask)
{
#if CONFIG_IDF_TARGET_ESP32
    mask |= 0x1f80ull;   /* GPIO 7-11: the SPI flash (6 is its clock) */
    mask |= 1ull << 6;
#endif
    reserved_pins = mask;
}

static struct espmod_instance *I(void *user) { return user; }

/* ---- pin ownership ---- */
static int claim(struct espmod_instance *in, int pin, bool output)
{
    if (pin < 0 || pin >= PINS || !GPIO_IS_VALID_GPIO(pin) || (output && !GPIO_IS_VALID_OUTPUT_GPIO(pin)))
        return ESPMOD_ERR_ARG;
    if (reserved_pins & (1ull << pin)) return ESPMOD_ERR_UNSUPPORTED;
    int rc = 0;
    portENTER_CRITICAL(&owner_lock);
    if (pin_owner[pin] && pin_owner[pin] != in) rc = ESPMOD_ERR_BUSY;
    else pin_owner[pin] = in;
    portEXIT_CRITICAL(&owner_lock);
    return rc;
}

static bool owns(struct espmod_instance *in, int pin)
{
    return pin >= 0 && pin < PINS && pin_owner[pin] == in;
}

static void unclaim(struct espmod_instance *in, int pin)
{
    portENTER_CRITICAL(&owner_lock);
    if (owns(in, pin)) pin_owner[pin] = NULL;
    portEXIT_CRITICAL(&owner_lock);
}

/* ---- GPIO ---- */
static int h_gpio_config(void *user, int pin, int mode, int pull, int level)
{
    int rc = claim(I(user), pin, mode != ESPMOD_GPIO_INPUT);
    if (rc) return rc;
    gpio_mode_t m = mode == ESPMOD_GPIO_INPUT ? GPIO_MODE_INPUT :
                    mode == ESPMOD_GPIO_OUTPUT ? GPIO_MODE_OUTPUT : GPIO_MODE_INPUT_OUTPUT;
    if (gpio_reset_pin(pin) != ESP_OK || gpio_set_direction(pin, m) != ESP_OK) return ESPMOD_ERR_IO;
    gpio_set_pull_mode(pin, pull == ESPMOD_GPIO_PULL_UP ? GPIO_PULLUP_ONLY :
                            pull == ESPMOD_GPIO_PULL_DOWN ? GPIO_PULLDOWN_ONLY : GPIO_FLOATING);
    if (mode != ESPMOD_GPIO_INPUT && gpio_set_level(pin, level != 0) != ESP_OK) return ESPMOD_ERR_IO;
    return 0;
}

static int h_gpio_read(void *user, int pin)
{
    if (!owns(I(user), pin)) return ESPMOD_ERR_ARG; /* configure it first */
    return gpio_get_level(pin);
}

static int h_gpio_write(void *user, int pin, int level)
{
    if (!owns(I(user), pin)) return ESPMOD_ERR_ARG;
    return gpio_set_level(pin, level != 0) == ESP_OK ? 0 : ESPMOD_ERR_IO;
}

/* ---- interrupts ----
 * The handler runs while flash is busy (ESP_INTR_FLAG_IRAM): gpio_get_level() may live in flash, the
 * register-level call is inline. */
static void IRAM_ATTR gpio_isr(void *arg)
{
    em_irq_slot_t *slot = arg;
    if (!slot->used) return;
    em_event_t ev = {.kind = EM_EV_CALL, .inst = slot->inst->id, .component = slot->component, .method = slot->method,
                     .value = gpio_ll_get_level(&GPIO, slot->pin)};
    BaseType_t higher = pdFALSE;
    if (!em_post_isr(&ev, &higher)) slot->inst->stats.irq_dropped++;
    else slot->inst->stats.irq_events++;
    if (higher) portYIELD_FROM_ISR();
}

static em_irq_slot_t *irq_find(struct espmod_instance *in, int pin)
{
    for (int i = 0; i < EM_IRQ_SLOTS; i++) if (in->irq[i].used && in->irq[i].pin == pin) return &in->irq[i];
    return NULL;
}

static int h_irq_register(void *user, int pin, int edge, int component, int method)
{
    struct espmod_instance *in = I(user);
    int rc = claim(in, pin, false);
    if (rc) return rc;
    if (irq_find(in, pin)) return ESPMOD_ERR_BUSY;
    em_irq_slot_t *slot = NULL;
    for (int i = 0; i < EM_IRQ_SLOTS && !slot; i++) if (!in->irq[i].used) slot = &in->irq[i];
    if (!slot) return ESPMOD_ERR_NOMEM;
    if (!isr_service) {
        esp_err_t e = gpio_install_isr_service(ESP_INTR_FLAG_IRAM);
        if (e != ESP_OK && e != ESP_ERR_INVALID_STATE) return ESPMOD_ERR_IO;
        isr_service = true;
    }
    gpio_num_t g = pin;
    gpio_int_type_t type = edge == ESPMOD_IRQ_RISING ? GPIO_INTR_POSEDGE :
                           edge == ESPMOD_IRQ_BOTH ? GPIO_INTR_ANYEDGE : GPIO_INTR_NEGEDGE;
    if (gpio_set_direction(g, GPIO_MODE_INPUT) != ESP_OK || gpio_set_intr_type(g, type) != ESP_OK) return ESPMOD_ERR_IO;
    slot->pin = pin;
    slot->component = component;
    slot->method = method;
    slot->used = true;
    if (gpio_isr_handler_add(g, gpio_isr, slot) != ESP_OK || gpio_intr_enable(g) != ESP_OK) {
        slot->used = false;
        return ESPMOD_ERR_IO;
    }
    return 0;
}

static int irq_remove(struct espmod_instance *in, em_irq_slot_t *slot)
{
    gpio_intr_disable(slot->pin);
    gpio_isr_handler_remove(slot->pin);
    slot->used = false;
    return 0;
}

static int h_irq_unregister(void *user, int pin)
{
    em_irq_slot_t *slot = irq_find(I(user), pin);
    return slot ? irq_remove(I(user), slot) : ESPMOD_ERR_NOTFOUND;
}

static int h_irq_enable(void *user, int pin, int enabled)
{
    em_irq_slot_t *slot = irq_find(I(user), pin);
    if (!slot) return ESPMOD_ERR_NOTFOUND;
    return (enabled ? gpio_intr_enable(pin) : gpio_intr_disable(pin)) == ESP_OK ? 0 : ESPMOD_ERR_IO;
}

/* ---- SPI ---- */
/* Bus ownership: first spi_open on a controller initialises it with its pins; later opens must use
 * the same pins. A controller somebody else initialised (the application) is shared as is. */
static struct { bool ours; int refs, sck, miso, mosi; } bus[3];
static SemaphoreHandle_t bus_lock;   /* the table above and the open/close sequences that change it */

static void bus_lock_take(void)
{
    portENTER_CRITICAL(&owner_lock);
    if (!bus_lock) bus_lock = xSemaphoreCreateMutex();
    portEXIT_CRITICAL(&owner_lock);
    xSemaphoreTake(bus_lock, portMAX_DELAY);
}
static void bus_lock_give(void) { xSemaphoreGive(bus_lock); }

static spi_host_device_t host_of(int bus_id)
{
#if CONFIG_IDF_TARGET_ESP32C6
    return SPI2_HOST;   /* no SPI3 */
#else
    return bus_id == 3 ? SPI3_HOST : SPI2_HOST;
#endif
}

static int spi_open_locked(void *user, const espmod_spi_config *c)
{
    struct espmod_instance *in = I(user);
    if (!c || c->size < sizeof(*c) || c->mode < 0 || c->mode > 3) return ESPMOD_ERR_ARG;
    spi_host_device_t host = host_of(c->bus);
    int idx = host == SPI2_HOST ? 1 : 2;
    em_spi_dev_t *dev = NULL;
    for (int i = 0; i < EM_SPI_DEVICES && !dev; i++) if (!in->spi[i].used) dev = &in->spi[i];
    if (!dev) return ESPMOD_ERR_NOMEM;
    int pins[] = {c->sck, c->miso, c->mosi, c->cs};
    for (int i = 0; i < 4; i++) {
        if (pins[i] < 0) { if (i == 0 || i == 3) return ESPMOD_ERR_ARG; continue; }
        int rc = claim(in, pins[i], i != 1);
        if (rc) return rc;
    }
    if (c->ready_pin >= 0) {
        int rc = claim(in, c->ready_pin, false);
        if (rc) return rc;
        gpio_reset_pin(c->ready_pin);
        gpio_set_direction(c->ready_pin, GPIO_MODE_INPUT);
    }
    if (bus[idx].refs && (bus[idx].sck != c->sck || bus[idx].miso != c->miso || bus[idx].mosi != c->mosi))
        return ESPMOD_ERR_BUSY;
    bool new_bus = false;
    if (!bus[idx].refs) {
        spi_bus_config_t bc = {.sclk_io_num = c->sck, .miso_io_num = c->miso, .mosi_io_num = c->mosi,
                               .quadwp_io_num = -1, .quadhd_io_num = -1,
                               .max_transfer_sz = CONFIG_ESPMOD_SPI_MAX_TRANSFER};
#if CONFIG_IDF_TARGET_ESP32
        /* Measured on two ESP32 boards: controller 3 on the automatically chosen DMA channel (channel 1 when it is the
         * only bus) clocks the transfer but never delivers the received bytes; with channel 2 it works. Fixed
         * channels, one per controller, also let both buses be open at once. */
        esp_err_t e = spi_bus_initialize(host, &bc, host == SPI3_HOST ? SPI_DMA_CH2 : SPI_DMA_CH1);
#else
        esp_err_t e = spi_bus_initialize(host, &bc, SPI_DMA_CH_AUTO);
#endif
        if (e != ESP_OK && e != ESP_ERR_INVALID_STATE) { ESP_LOGE(TAG, "spi bus %d: %s", c->bus, esp_err_to_name(e)); return ESPMOD_ERR_IO; }
        bus[idx].ours = e == ESP_OK;
        bus[idx].sck = c->sck; bus[idx].miso = c->miso; bus[idx].mosi = c->mosi;
        new_bus = true;
    }
    spi_device_interface_config_t dc = {.clock_speed_hz = c->hz ? (int)c->hz : 1000000, .mode = c->mode,
                                        .spics_io_num = c->cs, .queue_size = 1};
    spi_device_handle_t handle = NULL;
    dev->tx = heap_caps_malloc(CONFIG_ESPMOD_SPI_MAX_TRANSFER, MALLOC_CAP_DMA);
    dev->rx = heap_caps_malloc(CONFIG_ESPMOD_SPI_MAX_TRANSFER, MALLOC_CAP_DMA);
    dev->trans = calloc(1, sizeof(spi_transaction_t));
    if (!dev->tx || !dev->rx || !dev->trans || spi_bus_add_device(host, &dc, &handle) != ESP_OK) {
        free(dev->tx); free(dev->rx); free(dev->trans); dev->tx = dev->rx = NULL; dev->trans = NULL;
        if (new_bus && bus[idx].ours) spi_bus_free(host);
        if (new_bus) bus[idx].refs = 0;
        return ESPMOD_ERR_IO;
    }
    bus[idx].refs++;
    dev->handle = handle;
    dev->host = host;
    dev->ready_pin = c->ready_pin;
    dev->ready_level = c->ready_level != 0;
    dev->ready_timeout_ms = c->ready_timeout_ms ? c->ready_timeout_ms : 100;
    dev->used = true;
    return (int)(dev - in->spi);
}

static int h_spi_open(void *user, const espmod_spi_config *c)
{
    struct espmod_instance *in = I(user);
    if (!c || c->size < sizeof(*c)) return ESPMOD_ERR_ARG;
    int pins[5] = {c->sck, c->miso, c->mosi, c->cs, c->ready_pin};
    bool had[5];
    for (int i = 0; i < 5; i++) had[i] = owns(in, pins[i]);
    bus_lock_take();
    int rc = spi_open_locked(user, c);
    bus_lock_give();
    if (rc < 0) for (int i = 0; i < 5; i++) if (!had[i]) unclaim(in, pins[i]);   /* no pin stays claimed by a failed open */
    return rc;
}

static bool wait_ready(const em_spi_dev_t *dev)
{
    if (dev->ready_pin < 0) return true;
    int64_t deadline = esp_timer_get_time() + (int64_t)dev->ready_timeout_ms * 1000;
    int spins = 0;
    while (gpio_get_level(dev->ready_pin) != dev->ready_level) {
        if (esp_timer_get_time() >= deadline) return false;
        /* BUSY is normally gone within tens of microseconds: spin first, then give the CPU back. */
        if (++spins < 20) esp_rom_delay_us(10); else vTaskDelay(1);
    }
    return true;
}

static int h_spi_transfer(void *user, int handle, const uint8_t *tx, uint8_t *rx, size_t len)
{
    struct espmod_instance *in = I(user);
    if (handle < 0 || handle >= EM_SPI_DEVICES || !in->spi[handle].used) return ESPMOD_ERR_ARG;
    em_spi_dev_t *dev = &in->spi[handle];
    if (!tx || len == 0 || len > CONFIG_ESPMOD_SPI_MAX_TRANSFER) return ESPMOD_ERR_ARG;
    in->stats.spi_transfers++;
    if (!wait_ready(dev)) { in->stats.spi_errors++; return ESPMOD_ERR_BUSY; }
    /* Never spi_device_transmit(): a missing radio or a stuck bus must not wedge the application. */
    const TickType_t to = pdMS_TO_TICKS(100) ? pdMS_TO_TICKS(100) : 1;
    spi_transaction_t *t = dev->trans, *done = NULL;
    /* The driver keeps the pointer of a queued transaction: it stays in `dev` (not on this stack) and a
     * transfer that timed out is reclaimed before the next one reuses the buffers. */
    if (dev->pending) {
        if (spi_device_get_trans_result(dev->handle, &done, to) != ESP_OK) { in->stats.spi_errors++; return ESPMOD_ERR_BUSY; }
        dev->pending = false;
    }
    memcpy(dev->tx, tx, len);
    *t = (spi_transaction_t){.length = len * 8, .tx_buffer = dev->tx, .rx_buffer = dev->rx};
    if (spi_device_queue_trans(dev->handle, t, to) != ESP_OK) { in->stats.spi_errors++; return ESPMOD_ERR_IO; }
    dev->pending = true;
    if (spi_device_get_trans_result(dev->handle, &done, to) != ESP_OK || done != t) {
        in->stats.spi_errors++;
        return ESPMOD_ERR_IO;
    }
    dev->pending = false;
    /* Commands such as SX126x's keep BUSY asserted after CS rises: the next call must not start early. */
    if (!wait_ready(dev)) { in->stats.spi_errors++; return ESPMOD_ERR_BUSY; }
    if (rx) memcpy(rx, dev->rx, len);
    return 0;
}

static void spi_release_locked(struct espmod_instance *in, em_spi_dev_t *dev)
{
    int idx = dev->host == SPI2_HOST ? 1 : 2;
    if (dev->pending) {   /* the driver may still write the buffers: wait for it, and keep them if it never finishes */
        spi_transaction_t *done = NULL;
        if (spi_device_get_trans_result(dev->handle, &done, pdMS_TO_TICKS(1000)) != ESP_OK) {
            dev->used = false;   /* leaked on purpose: freeing them would let the driver write freed memory */
            return;
        }
    }
    spi_bus_remove_device(dev->handle);
    if (--bus[idx].refs == 0 && bus[idx].ours) { spi_bus_free(dev->host); bus[idx].ours = false; }
    free(dev->tx); free(dev->rx); free(dev->trans);
    memset(dev, 0, sizeof(*dev));
}

static void spi_release(struct espmod_instance *in, em_spi_dev_t *dev)
{
    bus_lock_take();
    spi_release_locked(in, dev);
    bus_lock_give();
}

static int h_spi_close(void *user, int handle)
{
    struct espmod_instance *in = I(user);
    if (handle < 0 || handle >= EM_SPI_DEVICES || !in->spi[handle].used) return ESPMOD_ERR_ARG;
    spi_release(in, &in->spi[handle]);
    return 0;
}

/* ---- I2C: one bus per call. Slow but stateless, and nothing stays claimed. ---- */
static int i2c_transfer_claimed(void *user, int port, int sda, int scl, uint32_t freq, uint8_t addr,
                          const uint8_t *tx, size_t tx_len, uint8_t *rx, size_t rx_len, uint32_t timeout_ms)
{
    struct espmod_instance *in = I(user);
    if ((tx_len && !tx) || (rx_len && !rx) || (!tx_len && !rx_len) || port < 0) return ESPMOD_ERR_ARG;
    int rc = claim(in, sda, true);
    if (!rc) rc = claim(in, scl, true);
    if (rc) return rc;
    i2c_master_bus_config_t bc = {.i2c_port = port, .sda_io_num = sda, .scl_io_num = scl,
                                  .clk_source = I2C_CLK_SRC_DEFAULT, .glitch_ignore_cnt = 7,
                                  .flags.enable_internal_pullup = true};
    i2c_master_bus_handle_t b;
    if (i2c_new_master_bus(&bc, &b) != ESP_OK) return ESPMOD_ERR_IO;
    i2c_device_config_t dc = {.dev_addr_length = I2C_ADDR_BIT_LEN_7, .device_address = addr,
                              .scl_speed_hz = freq < 10000 ? 10000 : freq};
    i2c_master_dev_handle_t d;
    esp_err_t e = i2c_master_bus_add_device(b, &dc, &d);
    if (e == ESP_OK) {
        int to = timeout_ms ? (int)timeout_ms : 1000;
        if (tx_len && rx_len) e = i2c_master_transmit_receive(d, tx, tx_len, rx, rx_len, to);
        else if (tx_len) e = i2c_master_transmit(d, tx, tx_len, to);
        else e = i2c_master_receive(d, rx, rx_len, to);
        i2c_master_bus_rm_device(d);
    }
    i2c_del_master_bus(b);
    unclaim(in, sda);
    unclaim(in, scl);
    return e == ESP_OK ? 0 : (e == ESP_ERR_TIMEOUT ? ESPMOD_ERR_TIMEOUT : ESPMOD_ERR_IO);
}

/* ---- ADC ---- */
static int h_adc_read(void *user, int pin, uint32_t ref_mv, int *raw, uint32_t *mv, int *unit_out, int *channel_out)
{
    (void)user;
    if (!raw || !mv) return ESPMOD_ERR_ARG;
    adc_unit_t unit;
    adc_channel_t channel;
    if (adc_oneshot_io_to_channel(pin, &unit, &channel) != ESP_OK) return ESPMOD_ERR_ARG;
    if (unit != ADC_UNIT_1) return ESPMOD_ERR_UNSUPPORTED; /* ADC2 conflicts with Wi-Fi */
    adc_oneshot_unit_handle_t h = NULL;
    adc_oneshot_unit_init_cfg_t init = {.unit_id = unit, .ulp_mode = ADC_ULP_MODE_DISABLE};
    if (adc_oneshot_new_unit(&init, &h) != ESP_OK) return ESPMOD_ERR_BUSY;
    adc_oneshot_chan_cfg_t cfg = {.atten = ADC_ATTEN_DB_12, .bitwidth = ADC_BITWIDTH_12};
    esp_err_t e = adc_oneshot_config_channel(h, channel, &cfg);
    if (e == ESP_OK) e = adc_oneshot_read(h, channel, raw);
    adc_oneshot_del_unit(h);
    if (e != ESP_OK) return ESPMOD_ERR_IO;
    *mv = (uint32_t)(((*raw < 0 ? 0 : *raw) * (uint64_t)ref_mv + 2047) / 4095);
    if (unit_out) *unit_out = (int)unit + 1;
    if (channel_out) *channel_out = (int)channel;
    return 0;
}

/* ---- WS2812 over RMT, one channel per write ---- */
static int h_rgbled_write(void *user, int pin, uint8_t r, uint8_t g, uint8_t b)
{
    struct espmod_instance *in = I(user);
    int rc = claim(in, pin, true);
    if (rc) return rc;
    rmt_channel_handle_t ch = NULL;
    rmt_tx_channel_config_t cc = {.gpio_num = pin, .clk_src = RMT_CLK_SRC_DEFAULT, .resolution_hz = 10000000,
                                  .mem_block_symbols = 64, .trans_queue_depth = 1};
    if (rmt_new_tx_channel(&cc, &ch) != ESP_OK) return ESPMOD_ERR_IO;
    /* 10 MHz: 0.1 us ticks. WS2812: bit0 = 0.3 high + 0.9 low, bit1 = 0.9 high + 0.3 low. */
    rmt_bytes_encoder_config_t ec = {
        .bit0 = {.level0 = 1, .duration0 = 3, .level1 = 0, .duration1 = 9},
        .bit1 = {.level0 = 1, .duration0 = 9, .level1 = 0, .duration1 = 3},
        .flags.msb_first = 1};
    rmt_encoder_handle_t enc = NULL;
    esp_err_t e = rmt_new_bytes_encoder(&ec, &enc);
    if (e == ESP_OK) e = rmt_enable(ch);
    if (e == ESP_OK) {
        uint8_t grb[3] = {g, r, b};
        rmt_transmit_config_t tc = {.loop_count = 0, .flags.eot_level = 0};
        e = rmt_transmit(ch, enc, grb, sizeof(grb), &tc);
        if (e == ESP_OK) e = rmt_tx_wait_all_done(ch, 100);
        rmt_disable(ch);
    }
    if (enc) rmt_del_encoder(enc);
    rmt_del_channel(ch);
    return e == ESP_OK ? 0 : ESPMOD_ERR_IO;
}

/* ---- wiring ---- */
void em_periph_init(void) {}

static int h_i2c_transfer(void *user, int port, int sda, int scl, uint32_t freq, uint8_t addr,
                          const uint8_t *tx, size_t tx_len, uint8_t *rx, size_t rx_len, uint32_t timeout_ms)
{
    struct espmod_instance *in = I(user);
    bool had_sda = owns(in, sda), had_scl = owns(in, scl);
    int rc = i2c_transfer_claimed(user, port, sda, scl, freq, addr, tx, tx_len, rx, rx_len, timeout_ms);
    if (!had_sda) unclaim(in, sda);   /* the pins are held for the call only */
    if (!had_scl) unclaim(in, scl);
    return rc;
}

void em_periph_fill(espmod_host *t)
{
    t->gpio_config = h_gpio_config; t->gpio_read = h_gpio_read; t->gpio_write = h_gpio_write;
    t->irq_register = h_irq_register; t->irq_unregister = h_irq_unregister; t->irq_enable = h_irq_enable;
    t->spi_open = h_spi_open; t->spi_transfer = h_spi_transfer; t->spi_close = h_spi_close;
    t->i2c_transfer = h_i2c_transfer; t->adc_read = h_adc_read; t->rgbled_write = h_rgbled_write;
}

void em_periph_release(struct espmod_instance *in)
{
    for (int i = 0; i < EM_IRQ_SLOTS; i++) if (in->irq[i].used) irq_remove(in, &in->irq[i]);
    for (int i = 0; i < EM_SPI_DEVICES; i++) if (in->spi[i].used) spi_release(in, &in->spi[i]);
    portENTER_CRITICAL(&owner_lock);
    for (int p = 0; p < PINS; p++) if (pin_owner[p] == in) pin_owner[p] = NULL;
    portEXIT_CRITICAL(&owner_lock);
}
