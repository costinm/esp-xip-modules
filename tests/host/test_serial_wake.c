#include <assert.h>
#include <stdio.h>
#include "xip_runtime.h"

static unsigned held, writes;
static bool worker_restored, receive_enabled;
void xip_serial_receive_enabled(bool enabled) { receive_enabled=enabled; }
static int reject;
void xip_power_activate(uint32_t ms) { assert(ms==5000); held++; }
int xip_serial_write_frame(uint32_t port,const void *data,size_t len)
{
    assert(worker_restored);
    assert(port==0xf8 && len==1 && *(const uint8_t *)data==1);
    writes++;
    return reject;
}
int main(void)
{
    xip_serial_note_wake(); /* one surviving byte or a physical UART wake edge */
    assert(held==1 && writes==0 && receive_enabled); /* no premature RX-task ACK */
    worker_restored=true;
    xip_serial_ack_awake(); assert(writes==1);
    xip_serial_ack_awake(); assert(writes==1);
    xip_serial_note_wake();xip_serial_note_wake();
    xip_serial_ack_awake();assert(held==3 && writes==2); /* coalesced */
    xip_serial_note_wake();reject=XIP_ERR_BUSY;
    xip_serial_ack_awake();assert(writes==3);
    reject=0;xip_serial_ack_awake();assert(writes==4); /* retained on failed TX */
    xip_serial_ack_awake();assert(writes==4);
    puts("serial byte wake and deferred awake ACK: ok");
}
