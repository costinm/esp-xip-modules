#define XIP_MODULE_HEAP_RESERVE 32768u
#define XIP_PACKET_HEAP_RESERVE ((CONFIG_XIP_PKT_HEAP_RESERVE > XIP_MODULE_HEAP_RESERVE) ? CONFIG_XIP_PKT_HEAP_RESERVE : XIP_MODULE_HEAP_RESERVE)

/* Reclaim only disposable packet cache. Live RX/TX leases are never revoked. */
static size_t module_heap_available(size_t size)
{
    allocation_reclaimed_bytes=0;
    size_t available = heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
    const size_t overhead = 31; /* managed allocation header and alignment */
    if (size > SIZE_MAX - XIP_MODULE_HEAP_RESERVE - overhead) return 0;
    const size_t wanted = XIP_MODULE_HEAP_RESERVE + overhead + size;
    if (available < wanted && pool) {
        allocation_reclaimed_bytes=(uint32_t)xip_pool_reclaim(pool, wanted - available);
        available = heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
    }
    return available;
}

/* Files survive individual module calls, so they are not charged to a module,
 * but must preserve the same host reserve in the internal capability heap. */
static void *memory_internal_alloc(size_t size)
{
    const unsigned caps = MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT;
    const size_t overhead = 31;
    if (size > SIZE_MAX - XIP_MODULE_HEAP_RESERVE - overhead) return NULL;
    const size_t wanted = size + XIP_MODULE_HEAP_RESERVE + overhead;
    size_t available = heap_caps_get_free_size(caps);
    if (available < wanted && pool) {
        xip_pool_reclaim(pool, wanted - available);
        available = heap_caps_get_free_size(caps);
    }
    if (available < wanted) return NULL;
    void *ptr = heap_caps_malloc(size, caps);
    if (ptr && heap_caps_get_free_size(caps) < XIP_MODULE_HEAP_RESERVE) {
        free(ptr); return NULL;
    }
    return ptr;
}

static void *h_mem_alloc(void *user, size_t size)
{
    /* Leave 32 KiB for the host. This checks current heap availability; other
     * firmware allocations can still consume that margin later. */
    const size_t margin = XIP_MODULE_HEAP_RESERVE;
    size_t available = module_heap_available(size);
    if (available <= margin || size > available - margin) {
        module_allocation_failed(I(user),size,1);return NULL;
    }
    void *ptr = xip_mem_alloc(&I(user)->mem, size);
    if (ptr && heap_caps_get_free_size(MALLOC_CAP_DEFAULT) < margin) {
        xip_mem_free(&I(user)->mem, ptr);
        module_allocation_failed(I(user),size,3);
        return NULL;
    }
    if (!ptr) module_allocation_failed(I(user),size,2);
    return ptr;
}
