/* Exercise the actual generic backend without an ESP or upload protocol. */
#include <assert.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include "xip.h"
#include "xip_pool.h"
#define ESP_OK 0
#define pdTRUE 1
#define pdPASS 1
#define CONFIG_XIP_WORKER_PRIORITY 5
#define XIP_EV_IO 9
static uint8_t flash[65536], packet[1204];
static int fail_create,callbacks,dropped;
static void (*task)(void *);
static void *task_arg;
static int worker_queue, registry_lock, critical, reenter, queue_failures;
static void completion_dispatch(void);
#define portENTER_CRITICAL(lock) do { (void)(lock); assert(!critical); critical = 1; } while (0)
#define portEXIT_CRITICAL(lock) do { (void)(lock); assert(critical); critical = 0; completion_dispatch(); } while (0)
typedef struct{uint32_t id;xip_held held;} instance;
#define I(p) ((instance *)(p))
typedef struct{int kind;uint32_t inst;int32_t component,method,value;uint64_t id;} xip_event_t;
static xip_event_t completion;
static int esp_flash_get_size(void *x,uint32_t *n){(void)x;*n=sizeof(flash);return ESP_OK;}
static int esp_flash_write(void *x,const void *data,uint32_t a,uint32_t n){(void)x;memcpy(flash+a,data,n);return ESP_OK;}
static int esp_flash_read(void *x,void *data,uint32_t a,size_t n){(void)x;memcpy(data,flash+a,n);return ESP_OK;}
static int esp_flash_erase_region(void *x,uint32_t a,uint32_t n){(void)x;memset(flash+a,255,n);return ESP_OK;}
static int xQueueSend(int q,const xip_event_t *ev,int wait){(void)q;(void)wait;if(queue_failures){queue_failures--;return 0;} completion=*ev;callbacks++;return pdTRUE;}
static int xTaskCreate(void (*entry)(void *),const char *name,int stack,void *arg,int priority,void *handle){(void)name;(void)stack;(void)priority;(void)handle;if(fail_create)return 0;task=entry;task_arg=arg;return pdPASS;}
static void run_task(void){void (*entry)(void *)=task;void *arg=task_arg;task=NULL;task_arg=NULL;assert(entry);entry(arg);}
static void vTaskDelay(int n){(void)n;if(task)run_task();}
static void vTaskDelete(void *x){(void)x;}
static void drop_slot(int n){assert(n==7);dropped++;}
static int h_pkt_span(void *u,uint32_t handle,uint8_t **data,size_t *cap){if(!xip_held_get(&I(u)->held,handle))return XIP_ERR_ARG;*data=packet;*cap=sizeof(packet);return 0;}
#include "../../components/xip_runtime/src/flash_hal.inc"
static instance *dispatch_instance;
static void completion_dispatch(void){if(reenter && callbacks){reenter=0;assert(atomic_load(&flash_owner)==0);assert(h_flash_erase(dispatch_instance,0x3000,0,4096,44,101,102)==0);}}
int main(void){
 instance in={.id=42};xip_held_init(&in.held,4);
 uint32_t pkt=xip_held_add(&in.held,7,64,0,XIP_HELD_OWNED);
 memset(flash,0,sizeof(flash));memcpy(packet+64,"payload",7);
 assert(h_flash_write(&in,0x1000,3,pkt,64,7,44,101,99)==0);
 assert(!xip_held_get(&in.held,pkt));assert(!dropped && !callbacks);
 assert(h_flash_erase(&in,0,0,4096,44,101,100)==XIP_ERR_BUSY);
 dispatch_instance=&in;reenter=1;queue_failures=1;
 run_task();assert(dropped==1 && callbacks==1);assert(task);run_task();assert(callbacks==2);assert(!memcmp(flash+0x1003,"payload",7));
 assert(completion.inst==42 && completion.id==102 && completion.value==0);
 uint8_t read[7];assert(h_flash_read(&in,0,0x1003,read,7)==7);assert(!memcmp(read,"payload",7));
 assert(h_flash_erase(&in,0,0,4096,44,101,100)==0);run_task();assert(flash[0]==255);
 assert(h_flash_erase(&in,1,0,4096,44,101,1)==XIP_ERR_ARG);
 assert(h_flash_erase(&in,65536,0,4096,44,101,1)==XIP_ERR_ARG);
 assert(h_flash_read(&in,UINT32_MAX,2,read,7)==XIP_ERR_ARG);
 pkt=xip_held_add(&in.held,7,64,0,XIP_HELD_OWNED);
 assert(h_flash_write(&in,0,0,pkt,1200,10,44,101,1)==XIP_ERR_ARG);assert(xip_held_get(&in.held,pkt));
 fail_create=1;assert(h_flash_write(&in,0,0,pkt,64,7,44,101,1)==XIP_ERR_NOMEM);assert(xip_held_get(&in.held,pkt));
 fail_create=0;int before=callbacks;assert(h_flash_write(&in,0x2000,0,pkt,64,7,44,101,1)==0);
 flash_release(42);assert(!task && callbacks==before && dropped==2);assert(flash[0x2000]==0);
 puts("physical flash HAL ownership/cancellation/ranges: ok");
}
