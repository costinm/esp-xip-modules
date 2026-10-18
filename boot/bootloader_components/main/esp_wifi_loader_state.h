#pragma once

/* State sector log (docs/boot.md). Two 4 KiB sectors used as an alternating
 * append-only record log. NOR flash turns bits 1->0 without an erase, so each
 * record goes into the next erased slot. The core is flash-agnostic (ops
 * vtable) so Stage2, the loader and host tests share it. */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define WL_STATE_SECTOR 4096u
#define WL_STATE_SIZE (2u * WL_STATE_SECTOR)

#define WL_REC_PIN 1u     /* 65-byte pinned public key */
#define WL_REC_MINVER 2u  /* u32 minimum Main version */
#define WL_REC_HEADER 3u  /* installed signed header (WL_HEADER_SIZE bytes) */
#define WL_REC_QIO 4u     /* u8 verdict */
#define WL_REC_MINVER_LOADER 5u /* u32 minimum loader version (bootstrap mode) */
#define WL_REC_MINVER_STAGE2 6u /* u32 minimum Stage2 version (bootstrap mode) */
#define WL_REC_MINVER_MODULE 7u /* u8 slot + u32: minimum version of one module slot (Rust crate) */
#define WL_MODULE_SLOTS 8


#define WL_QIO_OK 1u
#define WL_QIO_DIO_ONLY 2u

#define WL_PUBKEY_SIZE 65u
#define WL_HEADER_SIZE 216u

typedef struct {
    void *ctx;
    /* All return 0 on success. Offsets are relative to the 8 KiB area. */
    int (*read)(void *ctx, uint32_t off, void *buf, size_t len);
    int (*write)(void *ctx, uint32_t off, const void *buf, size_t len); /* 4-byte aligned */
    int (*erase)(void *ctx, uint32_t sector_off);
} wl_flash_ops_t;

typedef struct {
    bool has_pin, has_minver, has_header, has_minver_loader, has_minver_stage2;
    uint8_t pin[WL_PUBKEY_SIZE];
    uint32_t minver, minver_loader, minver_stage2;
    uint8_t header[WL_HEADER_SIZE];
    uint8_t qio; /* 0 none, WL_QIO_* */
    uint8_t has_module; /* bit per module slot with a recorded minimum version */
    uint32_t minver_module[WL_MODULE_SLOTS];
} wl_state_view_t;

typedef struct {
    wl_flash_ops_t ops;
    int active;       /* 0/1, or -1 when both sectors are blank */
    uint32_t seq;
    uint32_t used;    /* offset of the next free slot inside the active sector */
    bool dirty;       /* torn/corrupt record seen: compact before appending */
    wl_state_view_t view;
} wl_state_t;

/* Scan the sectors. Returns 0 even when blank. */
int wl_state_open(wl_state_t *st, const wl_flash_ops_t *ops);
/* Append one record, compacting into the other sector when needed. */
int wl_state_append(wl_state_t *st, uint8_t type, const void *data, size_t len);
/* Erase both sectors (explicit ownership change). */
int wl_state_erase_all(wl_state_t *st);
