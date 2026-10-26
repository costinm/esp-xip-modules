/* SPDX-License-Identifier: Apache-2.0 */
/* The CBOR envelope {1: component, 2: method, 3: id, 5: fields} (answer: {1, 2, 3, 6: result} or {1, 2, 3, 7: error}), parsed and
 * built by the runtime so that no module does: components/xip_runtime/src/xip_envelope.c, a pure part with no ESP-IDF that
 * tests/host/test_envelope.c runs on a PC. xip_runtime.c uses it for every packet that arrives on a module's port or as a 0x444d0002
 * datagram: the module receives an ordinary request (component, method, the fields map as `data`, the link and token to answer on)
 * and the runtime wraps what the module replies, so there is one answer shape and one error shape.
 *
 * A module answers with the result map (`hw.reply`) or with a bare negative integer, an `XIP_ERR_*` (`Hw::reply_error`); the
 * runtime turns the first into `6: result` and the second into `7: error`. */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    XIP_ENV_NOT_MAP = 0,        /* not a CBOR map: a raw frame, not an envelope */
    XIP_ENV_INVALID,            /* a map without a positive integer component and method, a bad id or fields that are not a map */
    XIP_ENV_REQUEST,
    XIP_ENV_ANSWER,             /* has a result (6) or an error (7): never dispatched as a request */
} xip_envelope_kind;

typedef struct {
    int32_t component, method;
    uint64_t id;                /* 0 when absent */
    bool has_id;
    const uint8_t *fields;      /* the map of field 5, in the input; a static empty map when absent */
    size_t fields_len;
} xip_envelope;

/* Classify and parse `cbor[0..len)`. `e` is filled for XIP_ENV_REQUEST (and, with the routing fields, for XIP_ENV_ANSWER and XIP_ENV_INVALID when they
 * could be read: component and method are 0 otherwise). */
xip_envelope_kind xip_envelope_parse(const uint8_t *cbor, size_t len, xip_envelope *e);

/* The head of an answer: `a4 01 <component> 02 <method> 03 <id> 06` (the result map follows) when `error` is 0, else the whole answer
 * `a4 01 .. 02 .. 03 .. 07 <error>` (`error` negative). Returns the length, or 0 when it does not fit `cap`. */
/* A complete directed CBOR envelope may vote for activity; broadcasts and
 * malformed/trailing data never do. Local target checks belong to the caller. */
bool xip_envelope_activity(const uint8_t *data, size_t len, bool directed);

size_t xip_envelope_head(uint8_t *out, size_t cap, int32_t component, int32_t method, uint64_t id, int32_t error);

/* Is `p[0..len)` exactly one negative integer (an error code in place of a result map)? */
bool xip_envelope_is_error(const uint8_t *p, size_t len, int32_t *error);

/* The requests of one module that wait for their answer: the answer to (dst, token, component, method) is wrapped with the request's id.
 * Newest first, so a request the module never answered cannot take another one's id. Full: the oldest is forgotten. */
#define XIP_ENV_PENDING 6
typedef struct { uint32_t dst; uint64_t token, id; int32_t component, method; } xip_env_slot;
typedef struct { xip_env_slot s[XIP_ENV_PENDING]; uint8_t n; } xip_env_table;
void xip_env_add(xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, uint64_t id);
/* The newest match: copied to `out`, true; false when none waits. */
bool xip_env_find(const xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, xip_env_slot *out);
/* Forget the newest match whose id is `id`. */
void xip_env_done(xip_env_table *t, uint32_t dst, uint64_t token, int32_t component, int32_t method, uint64_t id);

#ifdef __cplusplus
}
#endif
