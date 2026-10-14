/* SPDX-License-Identifier: Apache-2.0 */
/* Platform supplies INFO_LOCK/UNLOCK and info_memory_available(). */
typedef struct em_info_entry {
    struct em_info_entry *next;
    uint32_t owner;
    int32_t component, method;
    uint64_t updated_ms;
    size_t len;
    uint8_t data[];
} em_info_entry;
static em_info_entry *info_entries;

static bool info_map_valid(const uint8_t *data, size_t len)
{
    if (!data || !len) return false;
    em_cbor_r r; em_cbor_item it;
    em_cbor_reader(&r, data, len);
    if (em_cbor_next(&r, &it) || it.type != EM_CBOR_MAP) return false;
    if (it.n > r.len / 2) return false;
    size_t pending = 2 * it.n;
    while (pending) {
        if (em_cbor_next(&r, &it)) return false;
        pending--;
        size_t children = 0;
        if (it.type == EM_CBOR_MAP) {
            if (it.n > r.len / 2) return false;
            children = 2 * it.n;
        } else if (it.type == EM_CBOR_ARRAY) children = it.n;
        if (children > r.len || pending > r.len - children) return false;
        pending += children;
    }
    return r.len == 0;
}

static int info_publish(uint32_t owner, int32_t component, int32_t method,
                        const uint8_t *data, size_t len, uint64_t now_ms)
{
    if (component < 0 || method < 0 || (len && !info_map_valid(data, len))) return ESPMOD_ERR_ARG;
    if (len > SIZE_MAX - sizeof(em_info_entry)) return ESPMOD_ERR_NOMEM;
    INFO_LOCK();
    em_info_entry *entry = NULL;
    if (len) {
        size_t need = sizeof(*entry) + len;
        size_t available = info_memory_available();
        if (available < 32768 || need > available - 32768 || !(entry = malloc(need))) {
            INFO_UNLOCK(); return ESPMOD_ERR_NOMEM;
        }
        *entry = (em_info_entry){.owner = owner, .component = component, .method = method,
                                .updated_ms = now_ms, .len = len};
        memcpy(entry->data, data, len);
    }
    em_info_entry **p = &info_entries;
    while (*p && ((*p)->component != component || (*p)->method != method)) p = &(*p)->next;
    em_info_entry *old = *p;
    if (entry) { entry->next = old ? old->next : NULL; *p = entry; }
    else if (old) *p = old->next;
    INFO_UNLOCK();
    free(old);
    return ESPMOD_OK;
}

static void info_release(uint32_t owner)
{
    INFO_LOCK();
    em_info_entry **p = &info_entries;
    while (*p) {
        em_info_entry *e = *p;
        if (e->owner == owner) { *p = e->next; free(e); }
        else p = &e->next;
    }
    INFO_UNLOCK();
}

static void info_components(em_cbor_w *w)
{
    INFO_LOCK();
    size_t components = 0;
    for (em_info_entry *e = info_entries; e; e = e->next) {
        em_info_entry *p = info_entries;
        while (p != e && p->component != e->component) p = p->next;
        if (p == e) components++;
    }
    em_cbor_map(w, components);
    for (em_info_entry *e = info_entries; e; e = e->next) {
        em_info_entry *p = info_entries;
        while (p != e && p->component != e->component) p = p->next;
        if (p != e) continue;
        size_t methods = 0;
        for (p = e; p; p = p->next) if (p->component == e->component) methods++;
        em_cbor_uint(w, (uint32_t)e->component); em_cbor_map(w, methods);
        for (p = e; p; p = p->next) {
            if (p->component != e->component) continue;
            em_cbor_uint(w, (uint32_t)p->method); em_cbor_map(w, 2);
            em_cbor_uint(w, 1); em_cbor_uint(w, p->updated_ms);
            em_cbor_uint(w, 2); em_cbor_put(w, p->data, p->len);
        }
    }
    INFO_UNLOCK();
}
