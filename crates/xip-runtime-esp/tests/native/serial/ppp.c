/* Native control ports share the actual C PPP decoder with module traffic. */
#include <assert.h>
#include <string.h>
#include "xip_runtime.h"
extern const xip_serial_codec_t xip_ppp_codec;
static unsigned begin,commit,abort_count,controls;
static uint32_t last_port; static size_t last_len; static uint8_t last[32],packet[32];
int xip_serial_rx_begin(uint32_t port,xip_link_rx_buf *b) {
    assert(port==0xf7);begin++;b->data=packet;b->capacity=sizeof(packet);b->slot=1;return 0;
}
void xip_serial_rx_commit(uint32_t port,const xip_link_rx_buf *b,size_t n) {
    assert(port==0xf7&&b->slot==1);commit++;last_len=n;memcpy(last,packet,n);
}
void xip_serial_rx_abort(const xip_link_rx_buf *b) {(void)b;abort_count++;}
int xip_serial_frame_begin(uint32_t port,xip_link_rx_buf *b) { return xip_serial_rx_begin(port,b); }
void xip_serial_frame_finish(uint32_t port,const xip_link_rx_buf *b,size_t len,int outcome) {
    if(outcome)xip_serial_rx_abort(b);else xip_serial_rx_commit(port,b,len);
}
int xip_serial_control(uint32_t port,const uint8_t *data,size_t n) {
    controls++;last_port=port;last_len=n;memcpy(last,data,n);return 0;
}
int xip_serial_write(const void *data,size_t n) {(void)data;return (int)n;}
static void feed(const uint8_t *data,size_t n) {
    for(size_t i=0;i<n;i++)xip_ppp_codec.on_bytes(data+i,1);
}
int main(void) {
    const uint8_t data[]={0x7e,0xf7,1,0x7d,0x5e,0x7d,0x5d,0x7e};
    feed(data,sizeof(data));assert(begin==1&&commit==1&&last_len==3);
    assert(!memcmp(last,(uint8_t[]){1,0x7e,0x7d},3));
    const uint8_t wake[]={0x7e,0xf8,0x7e};feed(wake,sizeof(wake));
    assert(controls==1&&last_port==0xf8&&last_len==0&&begin==1);
    const uint8_t baud[]={0x7e,0xf9,0,0,0x0e,0x10,0,0x7e};feed(baud,sizeof(baud));
    assert(controls==2&&last_port==0xf9&&last_len==5&&begin==1);
    assert(!memcmp(last,baud+2,5));
    const uint8_t oversized[]={0x7e,0xf9,0,1,2,3,4,5,0x7e};feed(oversized,sizeof(oversized));assert(controls==2);
    const uint8_t malformed[]={0x7e,0xf9,0,0x7d,0x7e};feed(malformed,sizeof(malformed));assert(controls==2);
    const uint8_t partial[]={0x7e,0xf9,0,1};feed(partial,sizeof(partial));xip_ppp_codec.on_reset();
    feed(wake,sizeof(wake));assert(controls==3&&begin==1&&abort_count==0);
    feed(data,sizeof(data));assert(begin==2&&commit==2);
    const uint8_t shared[]={0x7e,0xf7,1,0x7e,0xf7,2,0x7e};feed(shared,sizeof(shared));
    assert(begin==4&&commit==4&&last_len==1&&last[0]==2);
}
