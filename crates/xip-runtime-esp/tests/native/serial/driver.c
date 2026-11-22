/* Actual serial owner, with pthread-backed SDK boundaries. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <limits.h>
#include <stddef.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>
#include <time.h>
#include "xip.h"
typedef struct { uint8_t *data; size_t capacity; int slot; } xip_link_rx_buf;
static void taskYIELD(void) {}
#define CONFIG_IDF_TARGET_ESP32C6 1
#define SOC_USB_SERIAL_JTAG_SUPPORTED 1
typedef struct { bool dp_pu,dp_pd,dm_pu,dm_pd; } usb_serial_jtag_pull_override_vals_t;
static struct { struct {bool dp_pullup,dp_pulldown,dm_pullup,dm_pulldown,pad_pull_override;} conf0; } USB_SERIAL_JTAG;
static void usb_serial_jtag_ll_phy_enable_pull_override(const usb_serial_jtag_pull_override_vals_t *p) {
    USB_SERIAL_JTAG.conf0.dp_pullup=p->dp_pu;USB_SERIAL_JTAG.conf0.dp_pulldown=p->dp_pd;
    USB_SERIAL_JTAG.conf0.dm_pullup=p->dm_pu;USB_SERIAL_JTAG.conf0.dm_pulldown=p->dm_pd;
    USB_SERIAL_JTAG.conf0.pad_pull_override=true;
}
static void usb_serial_jtag_ll_phy_disable_pull_override(void) { USB_SERIAL_JTAG.conf0.pad_pull_override=false; }
static int usb_serial_jtag_wait_tx_done(unsigned ticks) { (void)ticks;return 0; }
static int xip_wifi_nv(const char *key,size_t n,const uint8_t *value,size_t len,uint8_t *out,size_t cap) {
    (void)key;(void)n;(void)value;(void)len;(void)out;(void)cap;return -4;
}
static void xip_host_wake_window_value(int value) { (void)value; }
void xip_serial_note_wake(void) {}
#define CONFIG_XIP_SERIAL_STACK 4096
#define ESP_OK 0
#define ESP_ERR_INVALID_STATE 1
#define ESP_LOGE(tag,...) ((void)(tag))
#define ESP_LOGW(tag,...) ((void)(tag))
#define UART_NUM_0 0
#define UART_DATA_8_BITS 8
#define UART_PARITY_DISABLE 0
#define UART_STOP_BITS_1 1
#define UART_HW_FLOWCTRL_DISABLE 0
#define UART_SCLK_XTAL 0
#define UART_FIFO_OVF 2
#define UART_BUFFER_FULL 3
#define UART_EVENT_MAX 4
#define pdTRUE 1
#define pdPASS 1
#define portMAX_DELAY UINT32_MAX
#define pdMS_TO_TICKS(ms) (ms)
typedef int esp_err_t;
typedef uint32_t TickType_t;
typedef void *QueueHandle_t;
typedef struct { int type; } uart_event_t;
typedef struct { int baud_rate,data_bits,parity,stop_bits,flow_ctrl,source_clk; } uart_config_t;
typedef struct { size_t tx_buffer_size,rx_buffer_size; } usb_serial_jtag_driver_config_t;
typedef struct Task { pthread_t thread; pthread_mutex_t lock; pthread_cond_t wake; unsigned notified; void (*fn)(void*); void *arg; } *TaskHandle_t;
typedef struct { pthread_mutex_t lock; unsigned count; } *SemaphoreHandle_t;
static _Thread_local TaskHandle_t self;
static struct Task tasks[16];
static unsigned created, fail_at, installed, closes, rejected;
static atomic_uint received, tx_started, tx_done, default_received, rx_active, rx_peak;
static atomic_bool queued;
static pthread_mutex_t input_lock=PTHREAD_MUTEX_INITIALIZER;
static unsigned input;
static uint8_t input_wire[8192];
static size_t input_at, input_len;
static bool fail_usb;
static size_t usb_partial;
static uint8_t output_wire[8192];
static size_t output_len;
static void *run_task(void *p) { self=p; self->fn(self->arg); return NULL; }
static int xTaskCreate(void (*fn)(void*), const char *name, unsigned stack, void *arg, unsigned priority, TaskHandle_t *out) {
    (void)name; (void)priority; assert(stack==4096);
    if (fail_at && created+1==fail_at) return 0;
    TaskHandle_t t=&tasks[created++]; t->fn=fn; t->arg=arg; t->notified=0;
    pthread_mutex_init(&t->lock,NULL); pthread_cond_init(&t->wake,NULL); *out=t;
    assert(!pthread_create(&t->thread,NULL,run_task,t)); return 1;
}
static void vTaskDelete(void *unused) { assert(!unused); pthread_exit(NULL); }
static void vTaskDelay(unsigned ms) { usleep(ms*1000); }
static void xTaskNotifyGive(TaskHandle_t t) { pthread_mutex_lock(&t->lock); t->notified++; pthread_cond_signal(&t->wake); pthread_mutex_unlock(&t->lock); }
static unsigned ulTaskNotifyTake(int clear,unsigned wait) { (void)clear;(void)wait;pthread_mutex_lock(&self->lock);while(!self->notified)pthread_cond_wait(&self->wake,&self->lock);unsigned n=self->notified;self->notified=0;pthread_mutex_unlock(&self->lock);return n; }
static SemaphoreHandle_t xSemaphoreCreateCounting(unsigned max,unsigned initial) { (void)max;SemaphoreHandle_t s=calloc(1,sizeof(*s));pthread_mutex_init(&s->lock,NULL);s->count=initial;return s; }
static int xSemaphoreGive(SemaphoreHandle_t s) { pthread_mutex_lock(&s->lock);s->count++;pthread_mutex_unlock(&s->lock);return 1; }
static int xSemaphoreTake(SemaphoreHandle_t s,unsigned ms) { for(unsigned i=0;i<=ms;i++){pthread_mutex_lock(&s->lock);if(s->count){s->count--;pthread_mutex_unlock(&s->lock);return 1;}pthread_mutex_unlock(&s->lock);usleep(1000);}return 0; }
static void vSemaphoreDelete(SemaphoreHandle_t s) { pthread_mutex_destroy(&s->lock);free(s); }
static SemaphoreHandle_t xSemaphoreCreateMutex(void) { return xSemaphoreCreateCounting(1,1); }
static int usb_serial_jtag_is_connected(void) { return 1; }
static int usb_serial_jtag_driver_install(const usb_serial_jtag_driver_config_t *c) { assert(!installed);assert(c->rx_buffer_size==c->tx_buffer_size);installed=1;return 0; }
static void usb_serial_jtag_driver_uninstall(void) { assert(installed);assert(!atomic_load(&rx_active));installed=0; }
static int usb_serial_jtag_read_bytes(void *buf,size_t len,unsigned wait) {
    (void)len;unsigned n=atomic_fetch_add(&rx_active,1)+1;unsigned peak=atomic_load(&rx_peak);if(n>peak)atomic_store(&rx_peak,n);
    int got=0;for(unsigned i=0;i<=wait;i++){pthread_mutex_lock(&input_lock);if(input){input--;*(uint8_t*)buf=input_wire[input_at++];got=1;}pthread_mutex_unlock(&input_lock);if(got||!wait)break;usleep(1000);}
    atomic_fetch_sub(&rx_active,1);return got;
}
static int usb_serial_jtag_write_bytes(const void *data,size_t len,unsigned wait) { assert(wait==200);atomic_store(&tx_started,1);usleep(100000);
    if(fail_usb)return -1;
    size_t take=usb_partial&&len>usb_partial?usb_partial:len;
    assert(output_len+take<=sizeof(output_wire));memcpy(output_wire+output_len,data,take);output_len+=take;
    atomic_store(&tx_done,1);return (int)take; }
static int uart_param_config(int n,const uart_config_t *c){(void)n;(void)c;return 0;}
static int uart_driver_install(int n,int rx,int tx,int depth,QueueHandle_t *q,int flags){(void)n;(void)rx;(void)tx;(void)depth;(void)flags;*q=(void*)1;return 0;}
#define STUB(name) static int name(int n,...){(void)n;return 0;}
STUB(uart_driver_delete) STUB(uart_set_pin) STUB(uart_disable_tx_intr)
STUB(uart_set_rx_full_threshold) STUB(uart_set_rx_timeout) STUB(uart_set_always_rx_timeout)
STUB(uart_enable_rx_intr) STUB(uart_set_wakeup_threshold) STUB(esp_sleep_enable_uart_wakeup)
STUB(uart_disable_rx_intr) STUB(uart_read_bytes)
STUB(uart_flush_input) STUB(uart_get_buffered_data_len)
static unsigned physical_rate=115200, switches, tx_drains;
static bool fail_write, fail_drain;
static uint8_t ack_wire[32]; static size_t ack_len;
static int uart_write_bytes(int n,const void *data,size_t len) {
    (void)n;assert(physical_rate==115200);if(fail_write)return -1;
    size_t take=len>2?2:len;memcpy(ack_wire+ack_len,data,take);ack_len+=take;return (int)take;
}
static int uart_wait_tx_done(int n,unsigned ms) { (void)n;assert(ms==100);tx_drains++;return fail_drain?-1:0; }
static int uart_set_baudrate(int n,uint32_t baud) { (void)n;assert(tx_drains);physical_rate=baud;switches++;return 0; }
static uint64_t activity_time;
static int64_t esp_timer_get_time(void) { return (int64_t)activity_time*1000; }
static int xQueueReceive(void *q,void *event,unsigned wait){(void)q;(void)event;(void)wait;return 0;}
static void xQueueReset(void *q){(void)q;}
static void xQueueSend(void *q,void *event,unsigned wait){(void)q;(void)event;(void)wait;}
#define esp_rom_printf printf
static int (*cur_vprintf)(const char*,va_list);
/* IDF returns the previously installed handler; the serial owner chains to it. */
static int (*esp_log_set_vprintf(int (*fn)(const char*,va_list)))(const char*,va_list){
    int (*old)(const char*,va_list)=cur_vprintf;cur_vprintf=fn;return old;
}
static bool xPortInIsrContext(void){return false;}
typedef struct { void (*on_bytes)(const uint8_t*,size_t);void (*on_reset)(void);int (*tx)(uint32_t,const uint8_t*,size_t); } xip_serial_codec_t;
int xip_serial_rx_begin(uint32_t port,xip_link_rx_buf *buf) {(void)port;(void)buf;return 1;}
void xip_serial_rx_commit(uint32_t port,const xip_link_rx_buf *buf,size_t len) {(void)port;(void)buf;(void)len;}
void xip_serial_rx_abort(const xip_link_rx_buf *buf) {(void)buf;}
#include "serial_api.inc"
#include "activity.inc"
#include "serial_owner.inc"
static void default_rx(const uint8_t *data,size_t len){(void)data;atomic_fetch_add(&default_received,len);}
#ifndef SERIAL_PACKET_TEST
const xip_serial_codec_t xip_ppp_codec={.on_bytes=default_rx};
#else
#include "ppp_codec.inc"
#endif
static void feed(void){pthread_mutex_lock(&input_lock);if(!input){input_at=0;input_len=0;}input_wire[input_len++]=0x7e;input++;pthread_mutex_unlock(&input_lock);}
static void await(atomic_uint *value){for(unsigned i=0;i<1000&&!atomic_load(value);i++)usleep(1000);assert(atomic_load(value));}
static void rx(void *ctx,const uint8_t *data,size_t len){assert(ctx==(void*)42);assert(*data==0x7e);atomic_fetch_add(&received,len);}
static void legacy_tx(void *ctx){assert(ctx==(void*)42);if(atomic_exchange(&queued,false))assert(xip_serial_write("hello",5)==5);}
static void close_client(void *ctx){assert(ctx==(void*)42);assert(!installed);closes++;}
static void close_rejected(void *ctx){assert(ctx==(void*)84);assert(installed);rejected++;}
static void control_and_activity(void) {
    xip_power_clear_activity();activity_time=100;assert(xip_power_active_remaining_ms()==0);
    xip_power_activate(4000);activity_time=200;assert(xip_power_active_remaining_ms()==3900);
    xip_power_activate(60000);xip_power_activate(4000);assert(xip_power_active_remaining_ms()==60000);
    activity_time+=60000;assert(xip_power_active_remaining_ms()==0);
    activity_time=UINT32_MAX-10;xip_power_activate(30);activity_time+=20;assert(xip_power_active_remaining_ms()==10);
    xip_power_clear_activity();assert(xip_power_active_remaining_ms()==0);
    assert(xip_serial_control(0xf8,NULL,0)==0);assert(xip_power_active_remaining_ms()==5000);
    assert(xip_serial_control(0xf8,(uint8_t[]){1},1)==XIP_ERR_ARG);
    const uint8_t request[]={0,0,0x0e,0x10,0}; /* 921600 */
    assert(xip_serial_control(0xf9,request,5)==XIP_ERR_UNSUPPORTED); /* native USB */
    s.usb=false; /* Exercise physical UART boundary with synchronous generic owner. */
    for(unsigned failure=0;failure<3;failure++) {
        fail_write=failure==0;fail_drain=failure==1;ack_len=0;tx_drains=0;
        assert(xip_serial_control(0xf9,request,5)==0);
        assert(physical_rate==(failure==2?921600u:115200u));
        if(failure!=0) { const uint8_t expected[]={0x7e,0xf9,1,0,0x0e,0x10,0,0x7e};assert(ack_len==sizeof(expected));assert(!memcmp(ack_wire,expected,ack_len)); }
    }
    assert(switches==1);
    assert(xip_serial_control(0xf9,request,4)==XIP_ERR_ARG);
    assert(xip_serial_control(0xf9,(uint8_t[]){1,0,0x0e,0x10,0},5)==XIP_ERR_ARG);
    assert(xip_serial_request_baud(123456)==XIP_ERR_ARG);
    atomic_store(&s.pending_baud,115200);assert(xip_serial_request_baud(230400)==XIP_ERR_BUSY);
    assert(atomic_load(&s.pending_baud)==115200);atomic_store(&s.pending_baud,0);
    physical_rate=115200;tx_drains=0;assert(xip_serial_request_baud(230400)==0);assert(physical_rate==230400);
    const uint8_t req_2m[]={0,0,0x1e,0x84,0x80}; /* 2000000 */
    physical_rate=115200;fail_write=false;fail_drain=false;ack_len=0;tx_drains=0;
    assert(xip_serial_control(0xf9,req_2m,5)==0);
    assert(physical_rate==2000000u);
    { const uint8_t expected_2m[]={0x7e,0xf9,1,0,0x1e,0x84,0x80,0x7e};assert(ack_len==sizeof(expected_2m));assert(!memcmp(ack_wire,expected_2m,ack_len)); }
    s.usb=true; /* Restore driver identity before teardown. */
}

#ifndef SERIAL_PACKET_TEST
int main(void){
    assert(xip_serial_start()==0);feed();await(&default_received);control_and_activity();
    assert(xip_serial_open(921600,4612)==0);assert(s.running==false);
    xip_serial_client_t client={.ctx=(void*)42,.on_bytes=rx,.on_tx=legacy_tx,.on_close=close_client};
    assert(xip_serial_start_client(&client)==0);
    xip_serial_client_t other=client;other.ctx=(void*)84;other.on_close=close_rejected;
    assert(xip_serial_start_client(&other)==XIP_ERR_BUSY);assert(rejected==1);assert(closes==0);
    atomic_store(&queued,true);xip_serial_tx_ready();await(&tx_started);feed();await(&received);
    assert(!atomic_load(&tx_done)); /* RX progressed while the independent TX was blocked. */
    xip_serial_stop();assert(closes==1);assert(!installed);assert(atomic_load(&rx_peak)==1);
    xip_serial_stop();assert(closes==1); /* No double release. */
    assert(xip_serial_open(115200,4612)==0);fail_at=created+2;
    assert(xip_serial_start_client(&client)==XIP_ERR_NOMEM);assert(closes==2);assert(!installed);
    for(unsigned i=0;i<created;i++)assert(!pthread_join(tasks[i].thread,NULL));
    puts("serial handoff, full duplex, close ownership and creation failure passed");
}

#endif
