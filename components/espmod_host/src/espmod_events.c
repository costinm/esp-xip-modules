/* SPDX-License-Identifier: Apache-2.0 */
/* The last events modules emitted (events_info) and the state of the Wi-Fi (wifi_info), as CBOR records: the keys are the
 * `event` and `wifi` records of modules/mod_console/API.md, which the console module turns into text. Nothing here
 * formats a string. */
#include <string.h>
#include "espmod_cbor.h"
#include "internal.h"

#define EVENTS 16
#define VALUE_KEEP 32

static struct { char module[16]; uint16_t id; uint8_t type, len; uint8_t data[VALUE_KEEP]; } ring[EVENTS];
static unsigned ring_next;

void em_events_add(const char *module, uint16_t id, uint8_t type, const uint8_t *value, size_t len)
{
    unsigned i = ring_next++ % EVENTS;
    strlcpy(ring[i].module, module, sizeof(ring[i].module));
    ring[i].id = id;
    ring[i].type = type;
    ring[i].len = len > VALUE_KEEP ? VALUE_KEEP : (uint8_t)len;
    if (ring[i].len) memcpy(ring[i].data, value, ring[i].len);
}

/* {1: module, 2: id, 3: value as text (a text value as it is, anything else as hex)} per event, oldest first */
int em_events_info(uint8_t *out, size_t cap)
{
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    unsigned count = ring_next < EVENTS ? ring_next : EVENTS;
    em_cbor_array(&w, count);
    for (unsigned k = 0; k < count; k++) {
        unsigned i = (ring_next - count + k) % EVENTS;
        char hex[2 * VALUE_KEEP];
        const char *text = (const char *)ring[i].data;
        size_t n = ring[i].len;
        if (ring[i].type != ESPMOD_VALUE_TEXT) {
            for (size_t b = 0; b < n; b++) { hex[2 * b] = "0123456789abcdef"[ring[i].data[b] >> 4]; hex[2 * b + 1] = "0123456789abcdef"[ring[i].data[b] & 15]; }
            text = hex;
            n *= 2;
        }
        em_cbor_map(&w, 3);
        em_cbor_uint(&w, 1); em_cbor_text(&w, ring[i].module, strlen(ring[i].module));
        em_cbor_uint(&w, 2); em_cbor_uint(&w, ring[i].id);
        em_cbor_uint(&w, 3); em_cbor_text(&w, text, n);
    }
    int len = em_cbor_done(&w);
    return len < 0 ? ESPMOD_ERR_SIZE : len;
}

/* {1: state, 2: ssid, 3: ip, 4: rssi}; the last three once up */
int em_wifi_info(const espmod_host_hooks_t *hooks, uint8_t *out, size_t cap)
{
    int state = 0, rssi = 0;
    char ssid[33] = "", ip[16] = "";
    int rc = hooks->wifi_status(&state, ssid, ip, &rssi);
    if (rc) return rc;
    em_cbor_w w;
    em_cbor_init(&w, out, cap);
    em_cbor_map(&w, state == 2 ? 4 : 1);
    em_cbor_uint(&w, 1); em_cbor_uint(&w, (uint64_t)state);
    if (state == 2) {
        em_cbor_uint(&w, 2); em_cbor_text(&w, ssid, strlen(ssid));
        em_cbor_uint(&w, 3); em_cbor_text(&w, ip, strlen(ip));
        em_cbor_uint(&w, 4); em_cbor_int(&w, rssi);
    }
    int len = em_cbor_done(&w);
    return len < 0 ? ESPMOD_ERR_SIZE : len;
}
