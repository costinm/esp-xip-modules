/* SPDX-License-Identifier: Apache-2.0 */
#include <string.h>
#include "esp_wifi_loader_state.h"

#define SECTOR_MAGIC 0x31534c57u /* "WLS1" */
#define ACTIVE_MARK 0xa5a5a5a5u
#define SECTOR_HDR 16u /* magic, seq, active mark, pad */
#define REC_MAX 255u

static uint16_t crc16(uint16_t crc, const uint8_t *p, size_t n)
{
    while (n--) {
        crc ^= (uint16_t)(*p++) << 8;
        for (int i = 0; i < 8; ++i) crc = (crc & 0x8000) ? (crc << 1) ^ 0x1021 : (crc << 1);
    }
    return crc;
}

static uint32_t rec_word(uint8_t type, uint8_t len, const uint8_t *data)
{
    uint8_t h[2] = {type, len};
    uint16_t c = crc16(crc16(0xffff, h, 2), data, len);
    return (uint32_t)type | (uint32_t)len << 8 | (uint32_t)c << 16;
}

static uint32_t padded(uint32_t len) { return (len + 3u) & ~3u; }

static bool seq_newer(uint32_t a, uint32_t b) { return (int32_t)(a - b) > 0; }

static void apply(wl_state_view_t *v, uint8_t type, const uint8_t *d, uint8_t len)
{
    switch (type) {
    case WL_REC_PIN:
        if (len == WL_PUBKEY_SIZE) { memcpy(v->pin, d, len); v->has_pin = true; }
        break;
    case WL_REC_MINVER:
        if (len == 4) { memcpy(&v->minver, d, 4); v->has_minver = true; }
        break;
    case WL_REC_MINVER_LOADER:
        if (len == 4) { memcpy(&v->minver_loader, d, 4); v->has_minver_loader = true; }
        break;
    case WL_REC_MINVER_STAGE2:
        if (len == 4) { memcpy(&v->minver_stage2, d, 4); v->has_minver_stage2 = true; }
        break;
    case WL_REC_MINVER_MODULE:
        if (len == 5 && d[0] < WL_MODULE_SLOTS) {
            memcpy(&v->minver_module[d[0]], d + 1, 4);
            v->has_module |= (uint8_t)(1u << d[0]);
        }
        break;
    case WL_REC_HEADER:
        if (len == WL_HEADER_SIZE) { memcpy(v->header, d, len); v->has_header = true; }
        break;
    case WL_REC_QIO:
        if (len == 1) v->qio = d[0];
        break;
    }
}

/* Reads sector `idx`; returns true if it holds an active log. Fills view/used/dirty. */
static bool scan_sector(wl_state_t *st, int idx, uint32_t *seq, wl_state_view_t *view,
                        uint32_t *used, bool *dirty)
{
    uint32_t hdr[4];
    uint32_t base = (uint32_t)idx * WL_STATE_SECTOR;
    if (st->ops.read(st->ops.ctx, base, hdr, sizeof(hdr))) return false;
    if (hdr[0] != SECTOR_MAGIC || hdr[2] != ACTIVE_MARK) return false;
    *seq = hdr[1];
    memset(view, 0, sizeof(*view));
    *dirty = false;
    uint32_t off = SECTOR_HDR;
    while (off + 4 <= WL_STATE_SECTOR) {
        uint32_t w;
        if (st->ops.read(st->ops.ctx, base + off, &w, 4)) { *dirty = true; break; }
        if (w == 0xffffffffu) break;
        uint8_t type = w & 0xff, len = (w >> 8) & 0xff;
        uint32_t next = off + 4 + padded(len);
        uint8_t data[REC_MAX + 3];
        if (type == 0 || next > WL_STATE_SECTOR ||
            st->ops.read(st->ops.ctx, base + off + 4, data, padded(len)) ||
            rec_word(type, len, data) != w) {
            *dirty = true;
            break;
        }
        apply(view, type, data, len);
        off = next;
    }
    *used = off;
    return true;
}

int wl_state_open(wl_state_t *st, const wl_flash_ops_t *ops)
{
    memset(st, 0, sizeof(*st));
    st->ops = *ops;
    st->active = -1;
    for (int i = 0; i < 2; ++i) {
        uint32_t seq, used;
        bool dirty;
        wl_state_view_t v;
        if (!scan_sector(st, i, &seq, &v, &used, &dirty)) continue;
        if (st->active < 0 || seq_newer(seq, st->seq)) {
            st->active = i; st->seq = seq; st->used = used; st->dirty = dirty; st->view = v;
        }
    }
    return 0;
}

static int write_record(wl_state_t *st, uint32_t at, uint8_t type, const void *data, size_t len)
{
    uint8_t buf[4 + REC_MAX + 3];
    uint32_t w = rec_word(type, (uint8_t)len, data);
    memset(buf, 0xff, sizeof(buf));
    memcpy(buf, &w, 4);
    memcpy(buf + 4, data, len);
    return st->ops.write(st->ops.ctx, at, buf, 4 + padded(len));
}

/* Start `to` as the new active log containing the current view plus nothing
 * else; the old sector is erased only after the new one is marked active. */
static int compact(wl_state_t *st)
{
    int to = st->active < 0 ? 0 : 1 - st->active;
    uint32_t base = (uint32_t)to * WL_STATE_SECTOR;
    uint32_t seq = st->active < 0 ? 1 : st->seq + 1;
    if (st->ops.erase(st->ops.ctx, base)) return -1;
    uint32_t hdr[2] = {SECTOR_MAGIC, seq};
    if (st->ops.write(st->ops.ctx, base, hdr, sizeof(hdr))) return -1;
    uint32_t off = SECTOR_HDR;
    const wl_state_view_t *v = &st->view;
    struct { uint8_t type; const void *d; uint8_t len; bool has; } recs[] = {
        {WL_REC_PIN, v->pin, WL_PUBKEY_SIZE, v->has_pin},
        {WL_REC_MINVER, &v->minver, 4, v->has_minver},
        {WL_REC_MINVER_LOADER, &v->minver_loader, 4, v->has_minver_loader},
        {WL_REC_MINVER_STAGE2, &v->minver_stage2, 4, v->has_minver_stage2},
        {WL_REC_HEADER, v->header, WL_HEADER_SIZE, v->has_header},
        {WL_REC_QIO, &v->qio, 1, v->qio != 0},
    };
    for (size_t i = 0; i < sizeof(recs) / sizeof(recs[0]); ++i) {
        if (!recs[i].has) continue;
        if (write_record(st, base + off, recs[i].type, recs[i].d, recs[i].len)) return -1;
        off += 4 + padded(recs[i].len);
    }
    /* Module slots (written by the Rust crate) must survive a compaction done by the C code. */
    for (uint8_t slot = 0; slot < WL_MODULE_SLOTS; ++slot) {
        if (!(v->has_module & (1u << slot))) continue;
        uint8_t rec[5] = {slot};
        memcpy(rec + 1, &v->minver_module[slot], 4);
        if (write_record(st, base + off, WL_REC_MINVER_MODULE, rec, sizeof(rec))) return -1;
        off += 4 + padded(sizeof(rec));
    }
    uint32_t mark = ACTIVE_MARK;
    if (st->ops.write(st->ops.ctx, base + 8, &mark, 4)) return -1;
    int old = st->active;
    st->active = to; st->seq = seq; st->used = off; st->dirty = false;
    if (old >= 0) (void)st->ops.erase(st->ops.ctx, (uint32_t)old * WL_STATE_SECTOR);
    return 0;
}

int wl_state_append(wl_state_t *st, uint8_t type, const void *data, size_t len)
{
    if (type == 0 || len > REC_MAX) return -1;
    uint32_t need = 4 + padded(len);
    wl_state_view_t before = st->view;
    apply(&st->view, type, data, (uint8_t)len);
    if (st->active < 0 || st->dirty || st->used + need > WL_STATE_SECTOR) {
        /* compaction copies the updated view, which already holds this record */
        if (compact(st)) { st->view = before; return -1; }
        return 0;
    }
    if (write_record(st, (uint32_t)st->active * WL_STATE_SECTOR + st->used, type, data, len)) {
        st->view = before;
        st->dirty = true; /* the slot may be partly programmed */
        return -1;
    }
    st->used += need;
    return 0;
}

int wl_state_erase_all(wl_state_t *st)
{
    if (st->ops.erase(st->ops.ctx, 0) || st->ops.erase(st->ops.ctx, WL_STATE_SECTOR)) return -1;
    memset(&st->view, 0, sizeof(st->view));
    st->active = -1; st->seq = 0; st->used = 0; st->dirty = false;
    return 0;
}
