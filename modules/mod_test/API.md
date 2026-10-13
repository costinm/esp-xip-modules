# `test` API (2)

The device test module (see `docs/testing.md`). Start it with a mode (`start test serve`), then call it.

## 1. `echo` — answer with the same text and emit it as an event

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |

## 20. `pkt_echo` — send a packet back to its sender, unchanged but for the first byte

The packet itself goes back (no copy): `send(call.pkt, call.len, call.from, 2, 21, call.id)`. The answer is the call `21`.

## 22. `pkt_keep` — keep the packet past the call

Only for the tests of the packet rules: a kept packet that is still held when the module ends is counted as a leak.

## 23. `pkt_drop` — give the kept packet back

## 24. `mem_leak` — take memory and do not give it back

Only for the tests of the managed heap: the host frees the block when the module ends and reports the stop as failed (`ERR_LEAK`).

## 2. `info` — how often the handler ran

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `calls` | `uint` | Handler calls so far. |
| 2 | `mode` | `uint` | 0 selftest, 1 serve, 2 alarm, 3 gpio. |
