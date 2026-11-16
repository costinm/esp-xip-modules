//! Production global delivery FIFO: bursts, memory pressure and instance teardown.
use std::{fs, path::PathBuf, process::Command};
fn function(source: &str, signature: &str) -> String {
    let start = source.rfind(signature).unwrap();
    let body = start + source[start..].find('{').unwrap();
    let mut depth = 0;
    for (at, ch) in source[body..].char_indices() {
        if ch == '{' {
            depth += 1;
        }
        if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return source[start..=body + at].into();
            }
        }
    }
    panic!("unterminated {signature}")
}
#[test]
fn global_fifo_absorbs_bursts_and_releases_stopped_owners_without_payload_copies() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let runtime =
        fs::read_to_string(root.join("components/xip_runtime/src/xip_runtime.c")).unwrap();
    let queue =
        fs::read_to_string(root.join("components/xip_runtime/src/packet_queue.inc")).unwrap();
    let header = fs::read_to_string(root.join("components/xip_runtime/include/xip_runtime.h")).unwrap();
    let source_address = header.lines().find(|line| line.ends_with("} xip_udp_address;")).unwrap();
    let prepare = function(&runtime, "static bool packet_deliveries_prepare(void)");
    let deliver = function(&runtime, "static int deliver_checked(");
    let release = function(
        &runtime,
        "static void packets_release(struct xip_instance *in)",
    );
    let info = function(&runtime, "int xip_packet_queue_info(");
    let pressure = function(&runtime, "static void packet_rx_storage_shortage(void)");
    let handle = function(&runtime, "static void handle_packet(const packet_delivery *packet)");
    let out = root.join("target/packet-queue-native");
    fs::create_dir_all(&out).unwrap();
    for stats in [0, 1] {
        let fixture = format!(
            r#"
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include "xip_cbor.h"
#define CONFIG_XIP_PKT_QUEUE_INITIAL 32
#define CONFIG_XIP_PKT_SLOTS 64
#define CONFIG_XIP_PKT_KEEP 4
#define CONFIG_XIP_PKT_HEAP_RESERVE 16384
#define XIP_PACKET_HEAP_RESERVE 32768
#define XIP_PKT_SLOT_MIN 1704
#define XIP_MAX_INSTANCES 8
#define MALLOC_CAP_INTERNAL 1
#define MALLOC_CAP_8BIT 2
#define XIP_OK 0
#define XIP_ERR_NOTFOUND -7
#define XIP_ERR_BUSY -5
#define XIP_EV_PACKET 7
#define portENTER_CRITICAL(x) ((void)(x))
#define portEXIT_CRITICAL(x) ((void)(x))
{source_address}
{queue}
static packet_delivery_queue packet_queue;
static int registry_lock,worker_queue;
static void *pool;
static size_t free_bytes=1000000;
static int64_t clock_us=1000;
static size_t heap_caps_get_free_size(int caps){{(void)caps;return free_bytes;}}
static size_t pool_free_heap(void){{return free_bytes;}}
static int64_t esp_timer_get_time(void){{return clock_us;}}
static unsigned policy_keep,prepared,trimmed;
static void xip_pool_set_keep(void *p,unsigned keep){{(void)p;policy_keep=keep;}}
static void xip_pool_trim(void *p){{(void)p;trimmed++;}}
static unsigned xip_pool_reclaim(void *p,size_t bytes){{(void)p;(void)bytes;trimmed++;return 0;}}
static unsigned xip_pool_cached_available(void *p){{(void)p;return 4;}}
static void xip_pool_prepare_rx(void *p){{(void)p;prepared++;}}
static unsigned xip_pool_slots(void *p){{(void)p;return 64;}}
static unsigned xip_pool_allocated(void *p){{(void)p;return 4;}}
static unsigned xip_pool_available(void *p){{(void)p;return 64;}}
struct xip_instance{{uint32_t id;bool alive;uint64_t req_id;uint32_t req_has_id;struct{{unsigned pkt_dropped,pkt_received;}}stats;}};
static bool known_locked(const struct xip_instance *p){{return p && p->alive;}}
typedef struct{{int kind;}}xip_event_t;
static unsigned wakes;
static int xQueueSend(int q,const xip_event_t *ev,int timeout){{(void)q;assert(ev->kind==7 && timeout==0);wakes++;return 0;}}
static unsigned released[64];
static void drop_slot(int slot){{assert(slot>=0 && slot<64);released[slot]++;}}
#define XIP_CALL_LINK 8
#define XIP_CALL_ENVELOPE 64
#define XIP_ENV_REQUEST 1
typedef int xip_envelope_kind;
typedef struct{{int component,method;const uint8_t *fields;size_t fields_len;uint64_t id;bool has_id;}}xip_envelope;
static struct xip_instance *targets[2];
static struct xip_instance *find(uint32_t id){{for(unsigned i=0;i<2;i++)if(targets[i] && targets[i]->alive && targets[i]->id==id)return targets[i];return NULL;}}
static uint8_t storage[64][1704];
static uint8_t *xip_pool_slot(void *p,int slot){{(void)p;return storage[slot];}}
static uint32_t xip_packet_handle(void *p,int slot,uint16_t start,uint16_t len){{(void)p;assert(start==144 && len==8);return (uint32_t)slot+1;}}
static xip_envelope_kind xip_envelope_parse(const uint8_t *p,size_t len,xip_envelope *env){{(void)p;(void)len;(void)env;return 0;}}
static int c_power_received(void *ctx,const uint8_t *data,size_t len,int count){{(void)ctx;(void)data;(void)len;(void)count;return 0;}}
static void env_add(struct xip_instance *in,uint32_t from,uint64_t id,int component,int method,uint64_t request){{(void)in;(void)from;(void)id;(void)component;(void)method;(void)request;}}
static unsigned dispatches;
static int dispatch_packet(struct xip_instance *in,int component,int method,const uint8_t *data,size_t len,uint32_t h,uint32_t from,uint64_t id,uint32_t flags,int32_t value,uint32_t time_us,const xip_udp_address *src){{
    assert(src && src->port==3339 && src->scope==2 && src->ip[15]==9);
    assert(in->id==3 && component==48 && method==100 && h==45 && from==7 && id==44 && flags==XIP_CALL_LINK && value==9 && time_us==1234);
    assert(data==storage[44]+144 && len==8 && !memcmp(data,"borrowed",8));
    dispatches++;drop_slot((int)h-1);return 0;
}}
static bool settle(struct xip_instance *in,int rc){{(void)in;assert(rc==0);return false;}}
static void handle_unbound_packet(const packet_delivery *packet){{(void)packet;assert(0);}}

{prepare}
{deliver}
{release}
{info}
{pressure}
{handle}
int main(void){{
    free_bytes=CONFIG_XIP_PKT_HEAP_RESERVE;assert(!packet_deliveries_prepare());assert(packet_queue.capacity==0);
    free_bytes=1000000;assert(packet_deliveries_prepare());assert(packet_queue.capacity==32);
    struct xip_instance a={{.id=1,.alive=true}},b={{.id=2,.alive=true}};
    uint8_t payload[40][8];for(unsigned i=0;i<40;i++)memset(payload[i],(int)i,8);
    /* More than four deliveries to one module; a failed wake must not revoke an accepted reference. */
    for(unsigned i=0;i<32;i++)assert(deliver_checked(&a,0,(int)i,144,8,48,100,7,i,0,0,1234,NULL)==0);
    assert(packet_queue.count==32 && wakes==1);
    assert(deliver_checked(&a,0,32,144,8,48,100,7,32,0,0,1234,NULL)==XIP_ERR_BUSY);
#if CONFIG_XIP_PACKET_STATS
    assert(packet_queue.counters.high_water==32 && packet_queue.counters.queue_full==1);
#endif
    packet_delivery *old=packet_queue.entries;
    free_bytes=CONFIG_XIP_PKT_HEAP_RESERVE+32*sizeof(packet_delivery);
    assert(packet_deliveries_prepare());assert(packet_queue.entries==old && packet_queue.capacity==32);
    free_bytes=1000000;assert(packet_deliveries_prepare());assert(packet_queue.capacity==64);
    packet_delivery packet;
    for(unsigned i=0;i<5;i++){{assert(packet_queue_pop(&packet_queue,&packet));assert(packet.slot==(int)i && packet.id==i);assert(payload[packet.slot][0]==i);drop_slot(packet.slot);}}
    for(unsigned i=32;i<40;i++)assert(deliver_checked(i%2==0?&b:&a,0,(int)i,144,8,48,100,7,i,0,0,1234,NULL)==0);
    assert(deliver_checked(&a,99,40,144,8,48,100,7,40,0,0,1234,NULL)==XIP_ERR_NOTFOUND);
    pool=&a;a.alive=false;packets_release(&a);
    while(packet_queue_pop(&packet_queue,&packet)){{assert(packet.owner==2);drop_slot(packet.slot);}}
    for(unsigned i=0;i<40;i++)assert(released[i]==1);
    /* New instance id cannot inherit the old owner's queued frames. */
    a.id=3;a.alive=true;assert(deliver_checked(&a,1,40,144,8,48,100,7,40,0,0,1234,NULL)==XIP_ERR_NOTFOUND);
    assert(deliver_checked(&a,3,40,144,8,48,100,7,40,0,0,1234,NULL)==0);
    packets_release(&a);assert(released[40]==1 && packet_queue.count==0);
    for(unsigned i=0;i<40;i++)for(unsigned k=0;k<8;k++)assert(payload[i][k]==i);
    assert(deliver_checked(&a,3,41,144,8,48,100,7,41,0,0,1234,NULL)==0);
    assert(deliver_checked(&b,2,41,144,8,48,100,7,41,0,0,1234,NULL)==0);
    packets_release(&a);assert(released[41]==1);
    assert(packet_queue_pop(&packet_queue,&packet) && packet.owner==2);drop_slot(packet.slot);assert(released[41]==2);
    packet_delivery small[16],large[32];packet_delivery_queue wrapped={{0}};
    (void)packet_queue_resize(&wrapped,small,16);
    for(unsigned i=0;i<12;i++){{packet=(packet_delivery){{.id=i}};assert(packet_queue_push(&wrapped,&packet));}}
    for(unsigned i=0;i<9;i++){{assert(packet_queue_pop(&wrapped,&packet));assert(packet.id==i);}}
    for(unsigned i=12;i<24;i++){{packet=(packet_delivery){{.id=i}};assert(packet_queue_push(&wrapped,&packet));}}
    assert(packet_queue_resize(&wrapped,large,32)==small);
    for(unsigned i=9;i<24;i++){{assert(packet_queue_pop(&wrapped,&packet));assert(packet.id==i);}}

    unsigned wake_before=wakes;worker_queue=1;
    packet_rx_storage_shortage();packet_rx_storage_shortage();assert(wakes==wake_before+1);
    assert(packet_deliveries_prepare());
    assert(policy_keep==8 && prepared>0);
    clock_us+=250001;assert(packet_deliveries_prepare());assert(policy_keep==4 && trimmed==1);
    /* A later small burst must not inherit a large cache across a quiet/sleep gap. */
    packet_queue.rx_keep=32;packet_queue.rx_burst_until=clock_us-1;
    assert(deliver_checked(&a,3,42,144,8,48,100,7,42,0,0,1234,NULL)==0);
    assert(deliver_checked(&b,2,43,144,8,48,100,7,43,0,0,1234,NULL)==0);
    assert(packet_deliveries_prepare());assert(policy_keep==6 && trimmed==2);
    assert(packet_queue_pop(&packet_queue,&packet));drop_slot(packet.slot);
    assert(packet_queue_pop(&packet_queue,&packet));drop_slot(packet.slot);
    clock_us+=250001;assert(packet_deliveries_prepare());assert(policy_keep==4 && trimmed==3);

    targets[0]=&a;targets[1]=&b;
    memcpy(storage[44]+144,"borrowed",8);
    xip_udp_address src={{.ip={{0}},.port=3339,.scope=2}};src.ip[15]=9;
    assert(deliver_checked(&a,3,44,144,8,48,100,7,44,9,XIP_CALL_LINK,1234,&src)==0);
    assert(packet_queue_pop(&packet_queue,&packet));handle_packet(&packet);
    assert(dispatches==1 && released[44]==1 && a.stats.pkt_received==1);
    assert(deliver_checked(&a,3,45,144,8,48,100,7,45,9,XIP_CALL_LINK,1234,NULL)==0);
    assert(packet_queue_pop(&packet_queue,&packet));a.alive=false;handle_packet(&packet);
    assert(dispatches==1 && released[45]==1);
    uint8_t status[256];int n=xip_packet_queue_info(status,sizeof(status));assert(n>0);
    assert(fwrite(status,1,(size_t)n,stdout)==(size_t)n);
    free(packet_queue.entries);return 0;
}}
"#
        );
        let file = out.join(format!("queue-{stats}.c"));
        fs::write(&file, fixture).unwrap();
        let bin = out.join(format!("queue-{stats}"));
        let build = Command::new("cc")
            .args([
                "-std=gnu11",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fsanitize=address,undefined",
                "-g",
            ])
            .arg(format!("-DCONFIG_XIP_PACKET_STATS={stats}"))
            .arg("-I")
            .arg(root.join("include"))
            .arg("-I").arg(root.join("components/xip_runtime/src"))
            .arg(file)
            .arg("-o")
            .arg(&bin)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = Command::new(bin).output().unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        use xip_abi::cbor;
        assert_eq!(cbor::get_int(&run.stdout, 1).unwrap(), Some(0));
        assert_eq!(cbor::get_int(&run.stdout, 2).unwrap(), Some(64));
        assert_eq!(
            cbor::get_int(&run.stdout, 3).unwrap(),
            Some(if stats == 1 { 35 } else { 0 })
        );
        assert_eq!(cbor::get_int(&run.stdout,5).unwrap(),Some(if stats==1{1}else{0}));
        assert_eq!(
            cbor::get_int(&run.stdout, 6).unwrap(),
            Some(if stats == 1 { 1 } else { 0 })
        );
        assert_eq!(
            cbor::get_int(&run.stdout, 8).unwrap(),
            Some(if stats == 1 { 2 } else { 0 })
        );
    }
}
