/* Runs the actual socket/raw bearer with deterministic SDK and pool boundaries. */
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>
/* SOURCE_ADDRESS */
#include "xip_udp_peers.h"
#include "xip.h"
void xip_quic_udp_ensure(void);
#define CONFIG_XIP_QUIC_UDP_PORT 3339
#define ESP_OK 0
#define ESP_ERR_NO_MEM 0x101
#define XIP_LINK_TX_DONE 1
#define XIP_LINK_DOWN 0
#define XIP_LINK_UP 1
#define portMAX_DELAY -1
#define pdTRUE 1
#define WIFI_IF_STA 0
typedef unsigned wifi_interface_t;
typedef int SemaphoreHandle_t;
typedef unsigned xip_io_registration;
typedef struct {uint8_t *data;size_t len,headroom;} xip_link_packet;
typedef struct {uint8_t *data;size_t capacity;int slot;} xip_link_rx_buf;
static int sem=1;
static int xSemaphoreTake(int m,int ticks){(void)m;(void)ticks;return sem;}
static void xSemaphoreGive(int m){(void)m;}
static int xSemaphoreCreateMutex(void){return 1;}
static uint64_t esp_timer_get_time(void){return 1234;}
static void vTaskDelay(int n){(void)n;assert(0);}
static void lwip_socket_thread_init(void){}
static bool xip_connected(void){return true;}
static xip_io_registration xip_io_watch(int fd,int events,void (*cb)(unsigned,int,int16_t,void *),void *ctx)
{(void)fd;(void)events;(void)cb;(void)ctx;return 1;}
static void xip_io_clear(unsigned watch){(void)watch;}
static int xip_link_native_register(const char *name,unsigned mtu,unsigned flags,int (*tx)(void *,uint64_t,const xip_link_packet *),void *ctx)
{(void)name;(void)mtu;(void)flags;(void)tx;(void)ctx;return 7;}
static void xip_link_native_unregister(int link){assert(link==7);}
static void xip_link_native_set_state(int link,int value,int rate){assert(link==7);(void)value;(void)rate;}
static uint8_t lease[1704],mac[6]={0x10,0xbd,0xa3,0xac,0x5a,0x20};
static bool pool_available=true;
static void xip_link_native_rx_prepare(void){pool_available=true;}
static unsigned acquired,committed,aborted;
static uint64_t received_token;
static size_t received_len;
static int xip_link_native_rx_begin_cached(int link,xip_link_rx_buf *out)
{assert(link==7);if(!pool_available)return -1;acquired++;*out=(xip_link_rx_buf){lease+144,1500,1};return 0;}
static int xip_link_native_rx_begin(int link,xip_link_rx_buf *out)
{return xip_link_native_rx_begin_cached(link,out);}
static void xip_link_native_rx_abort(const xip_link_rx_buf *out){(void)out;aborted++;}
static void xip_link_native_rx_commit(int link,const xip_link_rx_buf *out,size_t n,uint64_t token,int signal)
{assert(link==7 && out->slot==1 && signal==0);committed++;received_len=n;received_token=token;}
static int esp_wifi_get_mac(unsigned interface,uint8_t *out){assert(interface==0);memcpy(out,mac,6);return 0;}
static int tx_result;
static uint8_t transmitted[1514];static size_t transmitted_len;
static int esp_wifi_internal_tx(unsigned interface,void *buffer,uint16_t len)
{assert(interface==0);transmitted_len=len;memcpy(transmitted,buffer,len);return tx_result;}
/* The discovery port path (mc_raw_rx): fakes for the runtime port registry. */
static unsigned port_begins,port_commits,port_aborts;
static uint32_t committed_handle,committed_port;
static size_t committed_len;
static xip_udp_address committed_src;
static int xip_port_rx_begin(uint32_t port,xip_link_rx_buf *out)
{(void)port;port_begins++;if(!pool_available)return -1;*out=(xip_link_rx_buf){lease+144,1500,2};return 0;}
static void xip_port_rx_abort(const xip_link_rx_buf *out){(void)out;port_aborts++;}
static void xip_port_rx_commit_udp(uint32_t handle,uint32_t port,const xip_link_rx_buf *out,size_t n,const xip_udp_address *src)
{(void)out;port_commits++;committed_handle=handle;committed_port=port;committed_len=n;if(src)committed_src=*src;}
/* ACTUAL_UDP_BEARER */
int main(void){
    struct udp_state *s=calloc(1,sizeof(*s));assert(s);mutex=1;state=s;
    s->fd=dup(STDOUT_FILENO);assert(s->fd>=0);s->link=7;s->peers.epoch=1;
    s->raw=calloc(XIP_UDP_PEERS,sizeof(*s->raw));assert(s->raw);atomic_init(&s->refs,1);
    uint8_t remote[6]={2,3,4,5,6,7},local[16],rip[16],frame[80]={0};
    udp6_local_ip(mac,local);udp6_local_ip(remote,rip);
    memcpy(frame+62,"hello",5);udp6_prefix(frame,5,remote,mac,rip,local,4444,3339);
    assert(!xip_quic_udp_raw_rx(0,frame,67)); /* Disabled is transparent. */
    xip_quic_udp_raw_mode(true);
    assert(xip_quic_udp_raw_rx(0,frame,67));assert(committed==1 && acquired==1);
    assert(received_len==5 && !memcmp(lease+144,"hello",5));
    uint8_t destination[22]={0};memcpy(destination,rip,16);destination[16]=4444>>8;destination[17]=4444&255;destination[21]=1;
    uint64_t configured=0;
    assert(xip_quic_udp_destination(destination,&configured)==0 && configured==received_token);
    s->peers.epoch=2;
    assert(xip_quic_udp_destination(destination,&configured)==0 && configured!=received_token);
    s->peers.epoch=1;
    destination[16]=destination[17]=0;
    assert(xip_quic_udp_destination(destination,&configured)==XIP_ERR_ARG);
    memset(frame,0,sizeof(frame)); /* SDK allocation may disappear immediately. */
    assert(!memcmp(lease+144,"hello",5));
    xip_link_packet pkt={lease+144,5,144};uint8_t *original=pkt.data;
    assert(udp_tx((void *)(uintptr_t)1,received_token,&pkt)==XIP_LINK_TX_DONE);
    assert(pkt.data==original && !memcmp(pkt.data,"hello",5));
    assert(transmitted_len==67 && udp6_selected(transmitted,67,rip,4444) && udp6_valid(transmitted,67));
    tx_result=ESP_ERR_NO_MEM;assert(udp_tx((void *)(uintptr_t)1,received_token,&pkt)==XIP_ERR_BUSY);
    assert(!memcmp(pkt.data,"hello",5)); /* Exact lease retained for retry. */
    pkt.headroom=61;assert(udp_tx((void *)(uintptr_t)1,received_token,&pkt)==XIP_ERR_SIZE);pkt.headroom=144;
    assert(udp_tx((void *)(uintptr_t)2,received_token,&pkt)==XIP_ERR_NOTFOUND);
    udp6_prefix(frame,5,remote,mac,rip,local,4444,3339);
    frame[62]^=1;assert(xip_quic_udp_raw_rx(0,frame,67));assert(acquired==1);
    udp6_prefix(frame,5,remote,mac,rip,local,4444,3339);
    pool_available=false;assert(xip_quic_udp_raw_rx(0,frame,67));assert(acquired==1);
    frame[20]=58;assert(!xip_quic_udp_raw_rx(0,frame,67)); /* NDP/lwIP. */
    frame[20]=17;sem=0;assert(!xip_quic_udp_raw_rx(0,frame,67));sem=1;
    /* The raw bypass is the QUIC port only: a datagram to any other port — the
     * discovery socket's port above all — must fall through to lwIP's socket
     * listeners, not be consumed here. Nothing else in the fixture changes. */
    udp6_prefix(frame,5,remote,mac,rip,local,4444,5227);
    assert(!xip_quic_udp_raw_rx(0,frame,67));assert(acquired==1);
    /* ...while the same frame on the QUIC port is still selected. */
    pool_available=true;
    udp6_prefix(frame,5,remote,mac,rip,local,4444,3339);
    assert(xip_quic_udp_raw_rx(0,frame,67));assert(acquired==2);
    /* Raw discovery ingress: a frame to the joined group on the sweep port is
     * consumed and delivered on the port registry path with the socket handle,
     * the source and the registry port — the module replies on that handle. */
    mc_mutex=(SemaphoreHandle_t)1;
    mc_state=calloc(1,sizeof(*mc_state));assert(mc_state);
    mc_state->fd=dup(STDOUT_FILENO);assert(mc_state->fd>=0);atomic_init(&mc_state->refs,1);
    uint8_t group[16];memcpy(group,XIP_MC_GROUP6,16);
    udp6_prefix(frame,5,remote,mac,rip,group,4444,5227);
    assert(xip_quic_udp_raw_rx(0,frame,67));assert(port_commits==1 && port_begins==1);
    assert(committed_handle==XIP_ADDR_UDP+(uint32_t)s->fd && committed_port==XIP_PORT_MC && committed_len==5);
    assert(committed_src.port==4444 && !memcmp(committed_src.ip,rip,16));
    /* A different port on the group address is not the discovery port. */
    udp6_prefix(frame,5,remote,mac,rip,group,4444,1234);
    assert(!xip_quic_udp_raw_rx(0,frame,67));assert(port_commits==1);
    /* Without the engine socket, discovery replies use the discovery socket. */
    state=NULL;
    udp6_prefix(frame,5,remote,mac,rip,group,4444,5227);
    assert(mc_raw_rx(0,frame,67));assert(port_commits==2);
    assert(committed_handle==XIP_ADDR_UDP+(uint32_t)mc_state->fd);
    state=s;
    assert(atomic_load(&s->refs)==1 && acquired==committed+aborted);
    xip_quic_udp_stop();assert(!state);
    xip_mc_stop();assert(!mc_state);
    assert(udp_tx((void *)(uintptr_t)1,received_token,&pkt)==XIP_ERR_NOTFOUND);
    return 0;
}
