# `wasm3` API (51)

Programs execute in the interpreter module. Inline execution and named program
loading return the same result/output/status map. Named programs are read through
the runtime files service; storage is supplied by mod_flash.

## 1. `run` — execute inline program content

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `program` | `bytes` | Inline program. |
| 2 | `export` | `string` | Default run. |
| 3 | `args` | `i64[]` | Up to four integer arguments. |
| 4 | `memory_cap` | `u32` | Default 16384 bytes. |

## 2. `run_file` — load and execute a named program

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 byte name in the files service. |
| 2 | `export` | `string` | Default run. |
| 3 | `args` | `i64[]` | Up to four integer arguments. |
| 4 | `memory_cap` | `u32` | Default 16384 bytes. |

The module reads ranges of at most 1024 bytes and pins the version returned by
the first read. It owns program memory only while loading/executing, releases
it on error/stop, and times out after 30 seconds. A second named load returns
BUSY. Inline calls remain available while a named load waits. Missing files,
version changes, unavailable storage and allocation failures return a negative
ABI error. Programs larger than one packet need no stream support in the caller.
