/* Production quarantine logic; replace only SDK retention/reset/logging. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <setjmp.h>
#include "xip_cbor.h"
#define RTC_NOINIT_ATTR
#define ESP_RST_POWERON 1
#define ESP_EARLY_LOGE(tag, ...) ((void)(tag))
#define MALLOC_CAP_DEFAULT 0
static int64_t esp_timer_get_time(void){return 2000;}
static size_t heap_caps_get_free_size(int caps){(void)caps;return 10000;}
static size_t heap_caps_get_largest_free_block(int caps){(void)caps;return 4000;}
static unsigned records;
static int xip_serial_write_frame(uint32_t port,const void *data,size_t n){assert(port==1 && data && n);records++;return 0;}
static int reason=ESP_RST_POWERON;
static jmp_buf reset;
static int esp_reset_reason(void) { return reason; }
static void esp_restart(void) { longjmp(reset,1); }
#define strlcpy test_strlcpy
static size_t test_strlcpy(char *out,const char *in,size_t cap) {
    size_t n=strlen(in); if(cap){size_t c=n<cap-1?n:cap-1;memcpy(out,in,c);out[c]=0;}return n;
}
struct xip_instance { char name[16]; uint32_t id; int component; struct {uint32_t used,quota;} mem; };
static struct xip_instance *I(void *user){return user;}
typedef struct {
    uint32_t version,size;
    void (*report)(void *,const uint8_t *,size_t);
    int (*read)(void *,uint8_t *,size_t,uint32_t *);
    void (*logged)(void *);
} xip_module_panic;
#include "../../components/xip_runtime/src/module_panic.inc"
static void reboot(bool power_on) { reason=power_on?ESP_RST_POWERON:3;module_panics_checked=false; }
static void panic_as(struct xip_instance *in,const uint8_t *data,size_t len) {
    if(!setjmp(reset)){module_panic_service.report(in,data,len);abort();}
}
int main(void) {
    struct xip_instance in={.name="ptest",.id=7,.component=44,.mem={10,100}};
    assert(!xip_module_panicked("ptest"));
    uint8_t text[512];memset(text,'a',sizeof text);
    module_allocation_failed(&in,248,1);
    panic_as(&in,text,sizeof text);
    assert(xip_module_panicked("ptest")); assert(!xip_module_panicked("flash"));
    assert(module_panics.count==1 && module_panics.message_len==192);
    reboot(false);
    assert(xip_module_panicked("ptest"));
    uint8_t out[256];uint32_t address=0;
    int n=module_panic_service.read(&in,out,sizeof out,&address);
    assert(n>0 && address==44);
    int64_t value=0;
    assert(!xip_cbor_get_int(out,(size_t)n,0,&value) && value==3);
    assert(!xip_cbor_get_int(out,(size_t)n,4,&value) && value==248);
    assert(!xip_cbor_get_int(out,(size_t)n,5,&value) && value==10000);
    assert(!xip_cbor_get_int(out,(size_t)n,6,&value) && value==4000);
    assert(records==2);
    module_panic_service.logged(&in);
    assert(module_panic_service.read(&in,out,sizeof out,&address)==0);
    assert(xip_module_panicked("ptest"));
    panic_as(&in,text,1); assert(module_panics.count==1);
    for(int i=1;i<32;i++){snprintf(in.name,sizeof in.name,"test%d",i);panic_as(&in,text,1);}
    assert(module_panics.count==32 && xip_module_panicked("unlisted"));
    reboot(true); assert(!xip_module_panicked("ptest"));assert(!xip_module_panicked("flash"));
    module_panics.count=999;reboot(false);assert(!xip_module_panicked("ptest"));
    puts("module panic: quarantine/reboot, durable acknowledgement, bounds and power-on clearing ok");
}
