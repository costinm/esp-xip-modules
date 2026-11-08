/* The state objects of a module: see xip_state.h. */
#include <stdlib.h>
#include <string.h>
#include "xip_state.h"

static bool same_name(const char *a, const char *b) { return strncmp(a, b, sizeof(((xip_retained_mod *)0)->name)) == 0; }

static xip_state_obj *find_obj(xip_state_obj *list, uint32_t key)
{
    for (; list; list = list->next) if (list->key == key) return list;
    return NULL;
}

static void unlink_obj(xip_state_obj **head, xip_state_obj *o)
{
    for (xip_state_obj **p = head; *p; p = &(*p)->next) if (*p == o) { *p = o->next; return; }
}

static void obj_free_block(xip_state_obj *o, xip_mem *mem)
{
    if (o->charged) xip_mem_free(mem, o->ptr);
    else free(o->ptr);
}

static void obj_free(xip_state_obj *o, xip_mem *mem)
{
    obj_free_block(o, mem);
    free(o);
}

static xip_retained_mod *find_mod(xip_retained_store *ret, const char *name, uint32_t version)
{
    if (!ret) return NULL;
    for (xip_retained_mod *m = ret->mods; m; m = m->next) if (m->version == version && same_name(m->name, name)) return m;
    return NULL;
}

static void remove_mod(xip_retained_store *ret, xip_retained_mod *m)
{
    for (xip_retained_mod **p = &ret->mods; *p; p = &(*p)->next) if (*p == m) { *p = m->next; break; }
    free(m);
}

static xip_state_obj *find_retained(xip_retained_store *ret, const char *name, uint32_t version, uint32_t key)
{
    xip_retained_mod *m = find_mod(ret, name, version);
    return m ? find_obj(m->objs, key) : NULL;
}

uint8_t *xip_state_set_root(xip_state_set *set, size_t size)
{
    xip_state_obj *o = calloc(1, sizeof(*o));
    if (!o) return NULL;
    o->ptr = calloc(1, size);
    if (!o->ptr) { free(o); return NULL; }
    o->key = 0;
    o->cls = XIP_STATE_INSTANCE;
    o->charged = false;
    o->fixed = true;
    o->size = size;
    o->next = set->objs;
    set->objs = o;
    return o->ptr;
}

/* An instance object. The root (key 0) is not charged to the quota: it is the module's own state, which was never counted. */
static int instance_resize(xip_state_set *set, xip_mem *mem, uint32_t key, size_t size, uint8_t **out)
{
    xip_state_obj *o = find_obj(set->objs, key);
    if (o && o->fixed) {                          /* the image's root keeps the size its header gives */
        if (size != o->size) return XIP_ERR_UNSUPPORTED;
        *out = o->ptr;
        return XIP_OK;
    }
    const bool charged = key != 0;
    const size_t old = o ? o->size : 0;
    if (size == 0) {
        if (o) { unlink_obj(&set->objs, o); obj_free(o, mem); }
        return XIP_OK;
    }
    if (o && size == old) { *out = o->ptr; return XIP_OK; }
    uint8_t *p;
    if (charged) {
        /* the quota counts the new block and not the old one, which is given back once the contents are copied */
        if (size > mem->quota || mem->used - old > mem->quota - size) return XIP_ERR_SIZE;
        mem->used -= (uint32_t)old;               /* xip_mem_alloc checks the quota without the old block */
        p = xip_mem_alloc(mem, size);
        mem->used += (uint32_t)old;
    } else {
        p = malloc(size);
    }
    if (!p) return XIP_ERR_NOMEM;
    xip_state_obj *target = o;
    if (!target) {
        target = calloc(1, sizeof(*target));
        if (!target) { if (charged) xip_mem_free(mem, p); else free(p); return XIP_ERR_NOMEM; }
        target->key = key;
        target->cls = XIP_STATE_INSTANCE;
        target->charged = charged;
        target->next = set->objs;
        set->objs = target;
    }
    memset(p, 0, size);
    if (old) memcpy(p, target->ptr, old < size ? old : size);
    if (old) obj_free_block(target, mem);
    target->ptr = p;
    target->size = size;
    *out = p;
    return XIP_OK;
}

static int retained_resize(xip_retained_store *ret, const char *name, uint32_t version, uint32_t key, size_t size, uint8_t **out)
{
    if (!ret) return XIP_ERR_UNSUPPORTED;
    xip_retained_mod *m = find_mod(ret, name, version);
    xip_state_obj *o = m ? find_obj(m->objs, key) : NULL;
    const size_t old = o ? o->size : 0;
    if (size == 0) {
        if (o) {
            unlink_obj(&m->objs, o);
            m->count--;
            ret->bytes -= old;
            obj_free(o, NULL);
            if (!m->objs) remove_mod(ret, m);
        }
        return XIP_OK;
    }
    if (o && size == old) { *out = o->ptr; return XIP_OK; }
    if (size > ret->cap || ret->bytes - old > ret->cap - size) return XIP_ERR_SIZE;
    if (!o && m && m->count >= XIP_STATE_RETAINED_OBJECTS) return XIP_ERR_SIZE;
    uint8_t *p = calloc(1, size);
    if (!p) return XIP_ERR_NOMEM;
    xip_retained_mod *fresh = NULL;
    xip_state_obj *fresh_obj = NULL;
    if (!m) {
        fresh = calloc(1, sizeof(*fresh));
        if (!fresh) { free(p); return XIP_ERR_NOMEM; }
        strncpy(fresh->name, name, sizeof(fresh->name) - 1);
        fresh->version = version;
        m = fresh;
    }
    if (!o) {
        fresh_obj = calloc(1, sizeof(*fresh_obj));
        if (!fresh_obj) { free(p); free(fresh); return XIP_ERR_NOMEM; }
        fresh_obj->key = key;
        fresh_obj->cls = XIP_STATE_RETAINED;
        fresh_obj->charged = false;
        o = fresh_obj;
    }
    if (old) memcpy(p, o->ptr, old < size ? old : size);
    if (old) free(o->ptr);
    o->ptr = p;
    o->size = size;
    if (fresh_obj) { o->next = m->objs; m->objs = o; m->count++; }
    if (fresh) { fresh->next = ret->mods; ret->mods = fresh; }
    ret->bytes = ret->bytes - old + size;
    *out = p;
    return XIP_OK;
}

int xip_state_resize(xip_state_set *set, xip_mem *mem, xip_retained_store *ret, const char *name, uint32_t version,
                     uint32_t key, uint32_t cls, size_t size, uint8_t **out)
{
    *out = NULL;
    if (cls == XIP_STATE_PSRAM || cls == XIP_STATE_RTC) return XIP_ERR_UNSUPPORTED;
    if (cls != XIP_STATE_INSTANCE && cls != XIP_STATE_RETAINED) return XIP_ERR_ARG;
    if (!set || !mem || !name) return XIP_ERR_ARG;
    const bool in_set = find_obj(set->objs, key) != NULL;
    const bool retained = !in_set && find_retained(ret, name, version, key) != NULL;
    if (cls == XIP_STATE_INSTANCE) {
        if (retained) return XIP_ERR_ARG;           /* the key is a retained object of this module */
        return instance_resize(set, mem, key, size, out);
    }
    if (key == 0 || in_set) return XIP_ERR_ARG;   /* the root is an instance object; a key of this instance is not retained */
    return retained_resize(ret, name, version, key, size, out);
}

int xip_state_get(xip_state_set *set, xip_retained_store *ret, const char *name, uint32_t version, uint32_t key,
                  uint8_t **out, size_t *size)
{
    xip_state_obj *o = set ? find_obj(set->objs, key) : NULL;
    if (!o && name) o = find_retained(ret, name, version, key);
    if (!o) return XIP_ERR_NOTFOUND;
    if (out) *out = o->ptr;
    if (size) *size = o->size;
    return XIP_OK;
}

void xip_state_release_instance(xip_state_set *set, xip_mem *mem)
{
    while (set->objs) {
        xip_state_obj *o = set->objs;
        set->objs = o->next;
        obj_free(o, mem);
    }
}

void xip_retained_forget_other_versions(xip_retained_store *ret, const char *name, uint32_t version)
{
    if (!ret) return;
    for (xip_retained_mod *m = ret->mods; m;) {
        xip_retained_mod *next = m->next;
        if (same_name(m->name, name) && m->version != version) {
            while (m->objs) {
                xip_state_obj *o = m->objs;
                m->objs = o->next;
                ret->bytes -= o->size;
                obj_free(o, NULL);
            }
            remove_mod(ret, m);
        }
        m = next;
    }
}
