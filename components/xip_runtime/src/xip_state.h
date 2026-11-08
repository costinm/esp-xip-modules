/* The state objects of a module (XIP_STATE_* classes, include/xip.h). Pure logic: no RTOS, so the host tests compile it.
 *
 * INSTANCE objects belong to one instance: they are counted in its managed heap (xip_mem) and freed when it is released. The
 * image's own state block (the root, key 0, when the header asks for one) is not counted: it is what the image has always had.
 * RETAINED objects belong to a module name and version: they live in a store the runtime keeps across instances, count against
 * the store's byte cap and XIP_STATE_RETAINED_OBJECTS per name, and are dropped when an instance with another version starts.
 * The runtime serializes every call on the store (the retained lock); this file takes no locks. */
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "xip.h"
#include "xip_pool.h"

#define XIP_STATE_RETAINED_OBJECTS 8    /* retained objects per module name */

typedef struct xip_state_obj {
    uint32_t key;
    uint32_t cls;                 /* XIP_STATE_INSTANCE or XIP_STATE_RETAINED */
    bool charged;                 /* from xip_mem (quota counted), given back with xip_mem_free; else malloc'd, given back with free */
    bool fixed;                   /* the image's root (state_bytes): its size is the header's, it is not resized */
    size_t size;
    uint8_t *ptr;
    struct xip_state_obj *next;
} xip_state_obj;

typedef struct {
    xip_state_obj *objs;          /* the instance objects, root included */
} xip_state_set;

typedef struct xip_retained_mod {
    char name[16];
    uint32_t version;
    size_t count;
    xip_state_obj *objs;
    struct xip_retained_mod *next;
} xip_retained_mod;

typedef struct {
    xip_retained_mod *mods;
    size_t bytes;                 /* of all retained objects */
    size_t cap;                   /* CONFIG_XIP_RETAINED_MAX */
} xip_retained_store;

/* The image's root: a zeroed fixed object of `size` bytes (> 0), not charged. NULL when there is no memory. */
uint8_t *xip_state_set_root(xip_state_set *set, size_t size);

/* XIP_OK with *out set (NULL after a free), or XIP_ERR_* (ARG, NOMEM, SIZE, UNSUPPORTED). `ret` may be NULL (no retained objects). */
int xip_state_resize(xip_state_set *set, xip_mem *mem, xip_retained_store *ret, const char *name, uint32_t version,
                     uint32_t key, uint32_t cls, size_t size, uint8_t **out);
/* XIP_OK, or XIP_ERR_NOTFOUND. Instance objects first, then the retained objects of name and version. */
int xip_state_get(xip_state_set *set, xip_retained_store *ret, const char *name, uint32_t version, uint32_t key,
                  uint8_t **out, size_t *size);
/* The instance ends: every instance object is freed (its managed heap balance then counts only what mem_alloc left). */
void xip_state_release_instance(xip_state_set *set, xip_mem *mem);
/* An instance of `name` starts with `version`: retained objects of the name with another version are dropped. */
void xip_retained_forget_other_versions(xip_retained_store *ret, const char *name, uint32_t version);
