/* SPDX-License-Identifier: Apache-2.0 */
/* The glue between the vendored interpreter (wasm3/) and the module ABI: the module cell and the state block.
 * Same cell as mod_berry's (modules/mod_berry/port/xip_berry.h): word 0 is the state block of the call being served,
 * word 1 is the runtime's `native` table (modules/common/xip_native_libc.h). A host build keeps the cell in a static
 * of its own name, because one test binary links more than one C module. */
#pragma once

#include <stdint.h>
#include "xip.h"
#include "xip_files_client.h"

#ifdef XIP_CELL_ADDR
#define XIP_CELL ((void **)(uintptr_t)(XIP_CELL_ADDR))
#else
extern void *xip_w3_cell[4];
#define XIP_CELL (xip_w3_cell)
#endif

#define WASM3_OUT_CAP 512    /* what the app logged, answered with the result (and sent to the host log) */
#define WASM3_RSP_CAP 1152   /* scratch for the CBOR answer being built (under the 2048-byte PKT_MAX) */
typedef struct { uint16_t len; char data[WASM3_OUT_CAP]; } wasm3_out;
typedef struct { uint16_t len; char data[WASM3_RSP_CAP]; } wasm3_rsp;
typedef struct {
    const xip_host *host;    /* the host of the call being served (the allocator and the app's imports run through it) */
    wasm3_out out;
    wasm3_rsp rsp;
    xip_file_load *load;
} wasm3_state;
