/* Run production poll_once with physical-interface connectivity and fake SDK poll. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <poll.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include "xip_lwip_io.h"
#define pdTRUE 1
#define pdPASS 1
#define portMAX_DELAY 0
#define pdMS_TO_TICKS(n) (n)
#define portMUX_INITIALIZER_UNLOCKED 0
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
#define sin6_len sin6_flowinfo
typedef int portMUX_TYPE;
typedef int*SemaphoreHandle_t;
static SemaphoreHandle_t xSemaphoreCreateMutex(void){return calloc(1,sizeof(int));}
static void xSemaphoreTake(SemaphoreHandle_t s,int timeout){(void)s;(void)timeout;}
static void vSemaphoreDelete(SemaphoreHandle_t s){free(s);}
static void xSemaphoreGive(SemaphoreHandle_t s){(void)s;}
static int xTaskCreate(void(*fn)(void*),const char*name,int stack,void*arg,int priority,void*handle){(void)fn;(void)name;(void)stack;(void)arg;(void)priority;(void)handle;return 1;}
static void vTaskDelay(int ticks){(void)ticks;}
static void vTaskDelete(void*p){(void)p;}
static bool physical_present;static unsigned initialized,cleaned,polls,callbacks;
static void*esp_netif_next_unsafe(void*old){(void)old;return physical_present?(void*)1:NULL;}
static void lwip_socket_thread_init(void){initialized++;}
static void lwip_socket_thread_cleanup(void){cleaned++;}
static int64_t esp_timer_get_time(void){return 1000000;}
static int lwip_socket(int a,int b,int c){(void)a;(void)b;(void)c;return -1;}
static int lwip_fcntl(int a,int b,int c){(void)a;(void)b;(void)c;return 0;}
static int lwip_bind(int fd,const struct sockaddr*a,socklen_t size){(void)fd;(void)a;(void)size;return 0;}
static int lwip_listen(int fd,int n){(void)fd;(void)n;return 0;}
static int lwip_close(int fd){(void)fd;return 0;}
static int lwip_accept(int fd,void*a,void*b){(void)fd;(void)a;(void)b;return -1;}
/* Replace the SDK poll boundary; poll_once remains production code. */
static int fake_poll(struct pollfd*fds,unsigned n,int timeout){(void)timeout;polls++;for(unsigned i=0;i<n;i++)fds[i].revents=POLLIN;return (int)n;}
#define lwip_poll fake_poll
#include "lwip_poll_runtime.inc"
static void ready(xip_io_registration reg,int fd,int16_t mask,void*ctx){(void)reg;assert(fd==7&&mask==POLLIN&&ctx==(void*)2);callbacks++;}
int main(void){
 bool init=false;xip_io_registration reg=xip_io_watch(7,POLLIN,ready,(void*)2);assert(reg);
 assert(poll_once(&init));assert(!polls&&!initialized&&!callbacks);
 physical_present=true;assert(poll_once(&init));assert(polls==1&&initialized==1&&callbacks==1);
 physical_present=true;assert(poll_once(&init));assert(polls==2&&initialized==1&&callbacks==2);
 xip_io_clear(reg);assert(!poll_once(&init));assert(!state);
 free(mutex);mutex=NULL;return 0;
}
