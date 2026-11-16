/* Production quarantine logic; replace only SDK retention/reset/logging. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <setjmp.h>
#include "xip_cbor.h"
#define XIP_ERR_ARG (-1)
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
struct xip_instance { char name[16]; uint32_t id; uint32_t version; int component; struct {uint32_t used,quota;} mem; };
static struct xip_instance *I(void *user){return user;}
typedef struct {
    uint32_t version,size;
    void (*report)(void *,const uint8_t *,size_t);
    int (*read)(void *,uint8_t *,size_t,uint32_t *);
    void (*logged)(void *);
} xip_module_panic;
typedef struct { uint32_t version,size; int (*clear)(void *,const uint8_t *,size_t); } xip_module_quarantine;
static void panic_flash_write(const uint8_t *data,size_t len){(void)data;(void)len;}
static void allocation_pressure_snapshot(uint32_t out[8]) { for(unsigned k=0;k<8;k++)out[k]=UINT32_MAX-k; }
#include "../../components/xip_runtime/src/module_panic.inc"
static void reboot(bool power_on) { reason=power_on?ESP_RST_POWERON:3;module_panics_checked=false; }
static void panic_as(struct xip_instance *in,const uint8_t *data,size_t len) {
    if(!setjmp(reset)){module_panic_service.report(in,data,len);abort();}
}
int main(void) {
    struct module_panic_v2 old={.magic=0x50414e32u,.count=1,.message_len=11,.alloc_size=80,.free=32748,.largest=27648,.used=1200,.quota=UINT32_MAX,.denial=1};
    memcpy(old.names[0],"nan",4);memcpy(old.last_module,"nan",4);memcpy(old.alloc_module,"nan",4);memcpy(old.message,"old message",11);
    memset(&retained_files,0,sizeof(retained_files));memcpy(&retained_files,&old,sizeof(old));reboot(false);
    assert(xip_module_panicked("nan"));assert(module_panics.magic==0x50414e35u);
    assert(module_panics.message_len==11 && !memcmp(module_panics.message,"old message",11));
    assert(!memcmp(module_panics.alloc_module,"nan",4) && module_panics.alloc_size==80 && module_panics.free==32748 && module_panics.denial==1);
    assert(!module_panics.logged && module_panics.module_version==0);
    reboot(true);

    struct xip_instance in={.name="ptest",.id=7,.component=44,.mem={10,100}};
    assert(!xip_module_panicked("ptest"));
    uint8_t text[1024];memset(text,'a',sizeof text);
    module_allocation_failed(&in,248,1);
    panic_as(&in,text,sizeof text);
    assert(xip_module_panicked("ptest")); assert(!xip_module_panicked("flash"));
    assert(module_panics.count==1 && module_panics.message_len==768);
    reboot(false);
    assert(xip_module_panicked("ptest"));
    uint8_t out[1024];uint32_t address=0;
    int n=module_panic_service.read(&in,out,sizeof out,&address);
    assert(n>0 && address==44);
    int64_t value=0;
    assert(!xip_cbor_get_int(out,(size_t)n,0,&value) && value==3);
    assert(!xip_cbor_get_int(out,(size_t)n,4,&value) && value==248);
    assert(!xip_cbor_get_int(out,(size_t)n,5,&value) && value==10000);
    assert(!xip_cbor_get_int(out,(size_t)n,6,&value) && value==4000);
    assert(records==2);
    assert(module_quarantine_service.clear(NULL,NULL,0)==XIP_ERR_ARG);
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"bad\0name",8)==XIP_ERR_ARG);
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"0123456789abcdef",16)==XIP_ERR_ARG);
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"ptest",5)==0);
    assert(!xip_module_panicked("ptest"));
    assert(module_panic_service.read(&in,out,sizeof out,&address)>0);
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"ptest",5)==0);
    reboot(false); assert(!xip_module_panicked("ptest"));
    panic_as(&in,text,1); assert(xip_module_panicked("ptest"));
    module_panic_service.logged(&in);
    assert(module_panic_service.read(&in,out,sizeof out,&address)==0);
    assert(xip_module_panicked("ptest"));
    assert(module_panics.message_len>0 && module_panics.logged);
    reboot(false);assert(module_panic_service.read(&in,out,sizeof out,&address)==0);
    assert(panic_record(out,sizeof out)>0);
    panic_as(&in,text,1); assert(module_panics.count==1);
    for(int i=1;i<32;i++){snprintf(in.name,sizeof in.name,"test%d",i);panic_as(&in,text,1);}
    assert(module_panics.count==32 && xip_module_panicked("unlisted"));
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"test15",6)==0);
    assert(module_panics.count==31 && !xip_module_panicked("test15"));
    assert(xip_module_panicked("test14") && xip_module_panicked("test16"));
    assert(module_quarantine_service.clear(NULL,(const uint8_t *)"test31",6)==0);
    assert(module_panics.count==30 && !xip_module_panicked("test31"));
    reboot(true); assert(!xip_module_panicked("ptest"));assert(!xip_module_panicked("flash"));
    module_panics.count=999;reboot(false);assert(!xip_module_panicked("ptest"));
    /* Full-size diagnostic keeps pressure outside encoding/padding scratch. */
    reboot(true);assert(!xip_module_panicked("ptest"));
    memcpy(in.name,"ptest",6);
    module_allocation_failed(&in,80,1);
    memset(text,'x',sizeof(text));panic_as(&in,text,sizeof(text));
    assert(module_panics.message_len==768 && module_panics.pressure[0]==UINT32_MAX);
    int encoded=panic_record(module_panics.wire,sizeof(module_panics.scratch));
    assert(encoded>768 && encoded<=992 && encoded+16<=1024);
    assert(module_panics.pressure[0]==UINT32_MAX && module_panics.pressure[6]==UINT32_MAX-6);
    allocation_reclaimed_bytes=1234;module_allocation_failed(&in,80,1);
    assert(module_panics.pressure[7]==0); /* unrelated later failure cannot alter retained panic */
    allocation_reclaimed_bytes=0;
    size_t files_offset=(uint8_t *)retained_files.state.files-(uint8_t *)&retained_files;
    memset(retained_files.state.files,0x5a,sizeof(retained_files.state.files));
    module_panics.magic=0x50414e34u;module_panics.pressure[0]=123;
    reboot(false);assert(xip_module_panicked("ptest"));assert(!module_panics.pressure[0]);
    assert(files_offset==sizeof(struct module_panic_state) && retained_files.state.files[0]==0x5a);
    puts("module panic: quarantine/reboot, durable acknowledgement, bounds and power-on clearing ok");
    return 0;
}
