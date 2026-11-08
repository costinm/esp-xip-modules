/* Compile production port subscription admission; actual frame fan-out is
 * exercised on C6 with recovery text and signed discovery on the shared port. */
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#define XIP_ERR_ARG -2
#define XIP_ERR_NOMEM -3
#define XIP_OK 0
#define XIP_PORTS_MAX 4
#define portENTER_CRITICAL(p) ((void)(p))
#define portEXIT_CRITICAL(p) ((void)(p))
static int registry_lock;
struct xip_instance {int component;uint32_t port;};
static struct {uint32_t port;int component;} ports[XIP_PORTS_MAX];
#include "../../components/xip_runtime/src/port_bind.inc"
int main(void)
{
    struct xip_instance flash={44,250},disc={47,250},nan={46,246},quic={48,244},extra={2,3},bad={0,250};
    assert(port_bind(&flash)==0);assert(port_bind(&disc)==0);
    assert(ports[0].port==250 && ports[0].component==44);
    assert(ports[1].port==250 && ports[1].component==47);
    assert(port_bind(&flash)==0);assert(port_bind(&disc)==0);
    assert(port_bind(&nan)==0);assert(port_bind(&quic)==0);
    assert(port_bind(&extra)==XIP_ERR_NOMEM);assert(port_bind(&bad)==XIP_ERR_ARG);
    puts("port subscriptions ok");
}
