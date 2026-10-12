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

## 2. `info` — how often the handler ran

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `calls` | `uint` | Handler calls so far. |
| 2 | `mode` | `uint` | 0 selftest, 1 serve, 2 alarm, 3 gpio. |
