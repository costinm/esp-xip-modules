static void *h_mem_alloc(void *user, size_t size)
{
    /* Leave 32 KiB for the host. This checks current heap availability; other
     * firmware allocations can still consume that margin later. */
    const size_t margin = 32768;
    size_t available = heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
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
