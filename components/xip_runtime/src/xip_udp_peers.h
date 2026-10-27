/* Internal UDP peer handles. No packet buffers or Rust/module pointers. */
#pragma once
#include <stdint.h>
#include <stdbool.h>
#include <string.h>
#define XIP_UDP_PEERS 16
#define XIP_UDP_PEER_IDLE_US (120ull*1000000ull)
/* xip_udp_address is `xip_runtime.h`'s: the public source descriptor. */
typedef struct { xip_udp_address addr; uint64_t seen_us; uint32_t generation; bool used; } xip_udp_peer;
typedef struct { uint32_t epoch; xip_udp_peer p[XIP_UDP_PEERS]; } xip_udp_peers;
static inline bool xip_udp_same(const xip_udp_address *a,const xip_udp_address *b)
{return a->port==b->port && a->scope==b->scope && !memcmp(a->ip,b->ip,16);}
static inline uint64_t xip_udp_peer_note(xip_udp_peers *t,const xip_udp_address *addr,uint64_t now)
{
    int free_at=-1;
    for(unsigned i=0;i<XIP_UDP_PEERS;i++){
        xip_udp_peer *p=&t->p[i];
        if(p->used && now-p->seen_us<XIP_UDP_PEER_IDLE_US && xip_udp_same(&p->addr,addr)){p->seen_us=now;return ((uint64_t)t->epoch<<32)|((uint64_t)p->generation<<8)|(i+1);}
        if((!p->used || now-p->seen_us>=XIP_UDP_PEER_IDLE_US) && p->generation<0xffffff && free_at<0)free_at=(int)i;
    }
    if(free_at<0)return 0;
    xip_udp_peer *p=&t->p[free_at];p->addr=*addr;p->generation++;p->seen_us=now;p->used=true;
    return ((uint64_t)t->epoch<<32)|((uint64_t)p->generation<<8)|((unsigned)free_at+1);
}
static inline bool xip_udp_peer_get(xip_udp_peers *t,uint64_t token,uint64_t now,xip_udp_address *out)
{
    unsigned i=(unsigned)(token&255);
    if((uint32_t)(token>>32)!=t->epoch || !i || i>XIP_UDP_PEERS)return false;
    xip_udp_peer *p=&t->p[i-1];
    if(!p->used || p->generation!=(uint32_t)((token>>8)&0xffffff) || now-p->seen_us>=XIP_UDP_PEER_IDLE_US)return false;
    p->seen_us=now;*out=p->addr;return true;
}
