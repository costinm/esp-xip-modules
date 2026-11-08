/* Actual C serial owner and PPP codec: pool ownership at the adapter boundary. */
#define SERIAL_PACKET_TEST
#include "driver.c"
#define PREFIX 196
static uint8_t pool[3][1704];
static atomic_uint refs[3], finished, committed, rx_open, client_closed;
static unsigned takes, completes, failed_completes;
static int rx_slot=-1;
struct source { unsigned slot; bool moved; };
static void feed_wire(const uint8_t *data,size_t len) {
    pthread_mutex_lock(&input_lock);
    if(!input){input_at=0;input_len=0;}
    assert(input_len+len<=sizeof(input_wire));memcpy(input_wire+input_len,data,len);input_len+=len;input+=len;
    pthread_mutex_unlock(&input_lock);
}
static void release_tx(void *owner,int outcome) {
    struct source *src=owner;assert(src->moved);assert(atomic_exchange(&refs[src->slot],0)==1);
    completes++;if(outcome)failed_completes++;atomic_fetch_add(&finished,1);
}
static void take_tx(void *ctx,void *from,xip_serial_packet_tx *packet) {
    assert(ctx==(void*)73);struct source *src=from;assert(!src->moved);assert(atomic_load(&refs[src->slot])==1);
    src->moved=true;takes++;
    *packet=(xip_serial_packet_tx){.data=pool[src->slot]+PREFIX,.len=5,.port=0xf7,.owner=src,.complete=release_tx};
}
static int begin_rx(void *ctx,uint32_t port,xip_serial_rx_buffer *buffer) {
    assert(ctx==(void*)73);if(port!=0xf7)return 1;assert(rx_slot==-1);
    for(unsigned i=0;i<3;i++) {unsigned free=0;if(atomic_compare_exchange_strong(&refs[i],&free,1)){
        rx_slot=(int)i;*buffer=(xip_serial_rx_buffer){.data=pool[i]+PREFIX,.capacity=1500};atomic_store(&rx_open,1);return 0;
    }}return XIP_ERR_NOMEM;
}
static void finish_rx(void *ctx,uint32_t port,size_t len,int outcome) {
    assert(ctx==(void*)73&&port==0xf7&&rx_slot>=0);
    if(!outcome){assert(len==3);assert(!memcmp(pool[rx_slot]+PREFIX,(uint8_t[]){7,0x7e,0x7d},3));atomic_fetch_add(&committed,1);}
    assert(atomic_exchange(&refs[rx_slot],0)==1);rx_slot=-1;atomic_store(&rx_open,0);
}
static void close_packets(void *ctx) {
    assert(ctx==(void*)73);assert(rx_slot==-1);for(unsigned i=0;i<3;i++)assert(!atomic_load(&refs[i]));
    assert(!installed);atomic_store(&client_closed,1);
}
static void wait_count(atomic_uint *value,unsigned target) {
    for(unsigned i=0;i<3000&&atomic_load(value)<target;i++) { usleep(1000); }
    assert(atomic_load(value)>=target);
}
static struct source source(unsigned slot) {
    assert(atomic_exchange(&refs[slot],1)==0);memcpy(pool[slot]+PREFIX,(uint8_t[]){1,0x7e,0x7d,4,5},5);
    return (struct source){.slot=slot};
}
int main(void) {
    assert(xip_serial_open(115200,6008)==0);
    xip_serial_packet_client client={.ctx=(void*)73,.rx_begin=begin_rx,.rx_finish=finish_rx,.close=close_packets};
    assert(xip_serial_start_packets(&client)==0);
    assert(xip_serial_write_frame(1,"bypass",6)==XIP_ERR_UNSUPPORTED);
    struct source first=source(0), busy=source(2);usb_partial=3;
    assert(xip_serial_submit_packet(&first,take_tx)==0);await(&tx_started);
    assert(xip_serial_submit_packet(&busy,take_tx)==XIP_ERR_BUSY);assert(!busy.moved&&takes==1);
    assert(!memcmp(pool[2]+PREFIX,(uint8_t[]){1,0x7e,0x7d,4,5},5));assert(atomic_exchange(&refs[2],0)==1);
    const uint8_t rx[]={0x7e,0xf7,7,0x7d,0x5e,0x7d,0x5d,0x7e};feed_wire(rx,sizeof(rx));await(&committed);
    assert(atomic_load(&refs[0])==1&&!atomic_load(&finished)); /* RX advances during current TX. */
    wait_count(&finished,1);assert(completes==1&&!failed_completes);
    const uint8_t expected[]={0x7e,0xf7,1,0x7d,0x5e,0x7d,0x5d,4,5,0x7e};
    assert(output_len==sizeof(expected)&&!memcmp(output_wire,expected,sizeof(expected)));
    fail_usb=true;struct source failure=source(0);assert(xip_serial_submit_packet(&failure,take_tx)==0);
    wait_count(&finished,2);assert(failed_completes==1);fail_usb=false;
    feed_wire((uint8_t[]){0x7e,0xf7,1},3);await(&rx_open);
    atomic_store(&tx_started,0);struct source cancelled=source(2);assert(xip_serial_submit_packet(&cancelled,take_tx)==0);await(&tx_started);
    xip_serial_stop();assert(atomic_load(&client_closed));assert(completes==3&&failed_completes==2);assert(atomic_load(&finished)==3);
    xip_serial_stop();assert(completes==3);for(unsigned i=0;i<created;i++)assert(!pthread_join(tasks[i].thread,NULL));
    puts("C packet owner: busy retains exact lease, one current frame, partial writes, concurrent RX, failure and close passed");
}
