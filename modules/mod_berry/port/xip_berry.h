/* SPDX-License-Identifier: Apache-2.0 */
/* The glue between the vendored interpreter (berry/) and the module ABI: the one writable word a
 * packed image may touch (the cell), and the state block that is the interpreter's home. */
#pragma once

#include <stdint.h>
#include "xip.h"
#include "xip_files_client.h"

/* The module cell. A flashed image is read-only flash with no .data and no .bss (tools/mkmod.py), so
 * a packed module reaches its writable word through 16 bytes of platform RAM reserved for its slot at
 * a fixed address - the same arena and the same trick crates/xip-abi/src/heap.rs uses for a Rust
 * module's allocator binding. The packer hands that address to the C compiler as
 * -DXIP_CELL_ADDR=0x... (see tools/dmod.py static_address). A host or linked build keeps the cell
 * in a static: one binary can serve several runtimes (the tests), and the entry is serialized by
 * crates/berry-c - one call live at a time, exactly as the device runtime serves them. Word 0
 * points at the state block of the call being served; the rest is free. */
#ifdef XIP_CELL_ADDR
#define XIP_CELL ((void **)(uintptr_t)(XIP_CELL_ADDR))
#else
extern void *xip_cell[4];
#define XIP_CELL (xip_cell)
#endif

/* What the interpreter remembers between calls; the state block is zeroed once per instance. */
#define BERRY_OUT_CAP 512    /* what the interpreter wrote: print, syntax errors, tracebacks */
#define BERRY_RSP_CAP 1152   /* scratch for the CBOR answer being built (under the 2048-byte PKT_MAX) */
typedef struct { uint16_t len; uint8_t full; char data[BERRY_OUT_CAP]; } berry_out;
typedef struct { uint16_t len; char data[BERRY_RSP_CAP]; } berry_rsp;
typedef struct {
    void *vm;                /* the bvm, made at start; null once stopped or if the heap refused it */
    const xip_host *host;    /* the host of the call being served (the allocator runs through it) */
    berry_out out;
    berry_rsp rsp;
    xip_file_load *load;
} berry_state;

/* Send everything captured in st->out to the host log - one log line per line of text - and reset
 * the capture. */
void xip_berry_flush(const xip_host *host, berry_state *st);
