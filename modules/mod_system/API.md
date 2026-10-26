# `system` API (49)

Memory, allocation tracing and task/stack diagnostics. Served directly by
mod_system without the console adapter. Power statistics, sleep policy and
PM-lock diagnostics belong to NAN. Native runtime mechanisms retain ownership
of heap capture and task enumeration. Diagnostic dump output goes to the device
log; a successful result means the diagnostic completed, not that a host saved it.

## 1. `heap` — memory usage snapshot

**Response:** [HeapInfo](#heapinfo)

Free, minimum-free and largest-block values, in bytes. Available even when
managed allocation is refused; the adapter uses stack storage only.

## 2. `heap_dump` — heap regions, blocks or allocation tracing

**Request:** [HeapDump](#heapdump)

Kinds: 0 memory regions, 1 block snapshot, 3 trace start, 4 trace dump, 5 trace
stop, 6 CBOR block records over native PPP. Use dump_tasks for task/stack usage.
Unsupported kinds fail. Trace availability depends on the firmware build.
Block snapshots use temporary runtime storage and may return a size error after
printing a truncated capture. Kind 6 emits records on PPP port 1 before replying.

## 3. `dump_tasks` — task and stack usage

Clients debugging scheduling or memory usage request task names, priorities and
remaining stack bytes. Runtime task enumeration uses temporary native storage.

# Types

## `HeapInfo`

**C type:** SystemHeapInfo

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `free` | `u32` | Free heap bytes. |
| 2 | `largest` | `u32` | Largest free block. |
| 3 | `min_free` | `u32` | Minimum free heap since boot. |
| 4 | `total` | `u32` | Total heap. |
| 5 | `internal_free` | `u32` | Free internal heap. |
| 6 | `internal_largest` | `u32` | Largest internal free block. |

## `HeapDump`

**C type:** SystemHeapDump

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `kind` | `i32` | Required: 0 regions, 1 blocks, 3/4/5 trace start/dump/stop, 6 CBOR blocks. |
