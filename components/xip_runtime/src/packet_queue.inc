/* One runtime FIFO of pool references. Callers serialize operations; allocation
 * and payload ownership remain outside this metadata-only queue. */
#include "xip_udp_peers.h"
typedef struct {
    uint32_t owner;
    int16_t slot;
    uint16_t start, len;
    int32_t component, method;
    uint32_t from;
    uint64_t id;
    int32_t value;
    uint32_t lflags, time_us;
    /* Source of a datagram received on a runtime-owned UDP socket (port 0: none). */
    xip_udp_address src;
} packet_delivery;
#if CONFIG_XIP_PACKET_STATS
typedef struct {
    uint32_t high_water, accepted, dispatched, queue_full, owner_stopped, rx_pool_empty;
} packet_queue_counters;
#endif
typedef struct {
    packet_delivery *entries;
    size_t capacity, head, count;
    int64_t rx_burst_until;
    unsigned rx_keep;
    bool grow, rx_pressure;
#if CONFIG_XIP_PACKET_STATS
    packet_queue_counters counters;
#endif
} packet_delivery_queue;
#if CONFIG_XIP_PACKET_STATS
#define PACKET_COUNT(q, field) ((q)->counters.field++)
#else
#define PACKET_COUNT(q, field) ((void)(q))
#endif
static bool packet_queue_push(packet_delivery_queue *q,const packet_delivery *packet)
{
    if(q->count==q->capacity){q->grow=true;PACKET_COUNT(q,queue_full);return false;}
    q->entries[(q->head+q->count)%q->capacity]=*packet;q->count++;
    PACKET_COUNT(q,accepted);
#if CONFIG_XIP_PACKET_STATS
    if(q->count>q->counters.high_water)q->counters.high_water=(uint32_t)q->count;
#endif
    return true;
}
static bool packet_queue_pop(packet_delivery_queue *q,packet_delivery *packet)
{
    if(!q->count)return false;
    *packet=q->entries[q->head];q->head=(q->head+1)%q->capacity;q->count--;
    return true;
}
/* Cleanup removes only this instance's references and preserves FIFO order. */
static bool packet_queue_remove_owner(packet_delivery_queue *q,uint32_t owner,packet_delivery *packet)
{
    for(size_t i=0;i<q->count;i++){
        size_t at=(q->head+i)%q->capacity;
        if(q->entries[at].owner!=owner)continue;
        *packet=q->entries[at];
        for(size_t k=i;k+1<q->count;k++)q->entries[(q->head+k)%q->capacity]=q->entries[(q->head+k+1)%q->capacity];
        q->count--;PACKET_COUNT(q,owner_stopped);return true;
    }
    return false;
}
/* Caller allocated replacement away from callbacks/critical sections. Only
 * descriptors move; the exact pool leases and payload addresses do not. */
static packet_delivery *packet_queue_resize(packet_delivery_queue *q,packet_delivery *entries,size_t capacity)
{
    if(!entries || capacity<q->count)return NULL;
    for(size_t i=0;i<q->count;i++)entries[i]=q->entries[(q->head+i)%q->capacity];
    packet_delivery *old=q->entries;q->entries=entries;q->capacity=capacity;q->head=0;q->grow=false;
    return old;
}
