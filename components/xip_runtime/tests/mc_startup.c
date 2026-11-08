#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>
#define ESP_OK 0
/* The production bind uses the wire port; the registry port (xip.h XIP_PORT_MC
 * 250) is a different number and must not leak into this fixture — redefining
 * XIP_PORT_MC here is what masked the wrong-port bind bug on devices. */
#define XIP_MC_UDP_PORT 5227
#define portMAX_DELAY -1
#define POLLIN 1
typedef struct { bool up; } esp_netif_t;
typedef struct {uint32_t addr[4];uint8_t zone;} esp_ip6_addr_t;
static esp_netif_t sta={true},ap={true},route={true};
static esp_netif_t *default_if,*sta_if=&sta,*ap_if=&ap,*joined;
static esp_netif_t *esp_netif_get_default_netif(void){return default_if;}
static esp_netif_t *esp_netif_get_handle_from_ifkey(const char *key){return !strcmp(key,"WIFI_STA_DEF")?sta_if:ap_if;}
static bool esp_netif_is_netif_up(esp_netif_t *n){assert(n);return n->up;}
static unsigned joins,closes;static int join_error,watch_error;
static int esp_netif_join_ip6_multicast_group(esp_netif_t *n,const esp_ip6_addr_t *group){
 const uint8_t expected[16]={0xff,2,0,0,0,0,0,0,0,0,0,0,0,0,0x52,0x27};
 assert(n && n->up);assert(!memcmp(group->addr,expected,16));assert(group->zone==0);joined=n;joins++;return join_error;
}
static bool xip_connected(void){return true;}
static int mc_mutex;
static int xSemaphoreCreateMutex(void){return 1;}
static void xSemaphoreTake(int m,int wait){assert(m==1);(void)wait;}
static void xSemaphoreGive(int m){assert(m==1);}
static void lwip_socket_thread_init(void){}
static int fake_socket(int domain,int type,int protocol){assert(domain==AF_INET6 && type==SOCK_DGRAM && protocol==0);return 7;}
static int fake_setsockopt(int f,int level,int opt,const void *value,socklen_t n){assert(f==7);(void)level;(void)opt;(void)value;(void)n;return 0;}
static int fake_fcntl(int f,int cmd,int flags){assert(f==7 && cmd==F_SETFL && flags==O_NONBLOCK);return 0;}
static int fake_bind(int f,const struct sockaddr *addr,socklen_t n){assert(f==7 && n==sizeof(struct sockaddr_in6));assert(ntohs(((const struct sockaddr_in6 *)addr)->sin6_port)==XIP_MC_UDP_PORT);return 0;}
static int fake_close(int f){assert(f==7);closes++;return 0;}
#define socket fake_socket
#define setsockopt fake_setsockopt
#define fcntl fake_fcntl
#define bind fake_bind
#define close fake_close
static void mc_ready(void){}
static unsigned xip_io_watch(int fd,int events,void (*cb)(void),void *ctx){assert(fd==7 && events==POLLIN && cb==mc_ready && !ctx);return watch_error?0:1;}
struct mc_state {int fd;unsigned watch;_Atomic unsigned refs;};
static struct mc_state *mc_state;
/* ACTUAL_GROUP */
/* ACTUAL_ENSURE */
static void clear(void){free(mc_state);mc_state=NULL;}
int main(void){
 /* Association is up but DHCP has not installed a default route. */
 xip_mc_ensure();assert(mc_state && joined==&sta && joins==1);clear();
 default_if=&route;xip_mc_ensure();assert(mc_state && joined==&route);clear();
 route.up=false;sta.up=false;xip_mc_ensure();assert(mc_state && joined==&ap);clear();
 unsigned before=joins,closed=closes;ap_if=sta_if=default_if=NULL;
 xip_mc_ensure();assert(!mc_state && joins==before && closes==closed+1);
 sta_if=&sta;sta.up=true;join_error=1;closed=closes;xip_mc_ensure();assert(!mc_state && closes==closed+1);
 join_error=0;watch_error=1;closed=closes;xip_mc_ensure();assert(!mc_state && closes==closed+1);
 return 0;
}
