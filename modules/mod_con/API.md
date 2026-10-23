# `console` API (20)

Binary device control with a retained text interface. Binary methods operate directly on CBOR fields; `parse` and `render` remain compatible while callers migrate. Its manifest names a port (the serial line, a link
like any other): each line arrives as a frame of that link (the line as bytes) and the text it answers goes back to the source of the line, with
the `from` and `id` the frame carried, so a line from the network is answered on the network.
A line is one of the commands below or `component.method key=value ...` (the spelling of the ssh-mesh `mesh` CLI), which
becomes the tagged-CBOR request of the module `component` (names and tags from the `API.md` files of the modules,
`mesh-api-gen`, `scripts/gen.sh`), and is sent as a packet with an id. Nothing waits for an answer: whatever packets modules send to the
console, one or many, answers or not, are printed as they arrive. Dispatch forwards the component, method and fields to the module handler; it does not re-read or enforce a manifest method list. `parse` (1) and `render`
(2) below are the two halves of that, for a caller that has its own transport. Binary clients use the methods below directly, or `dispatch` with the shared tagged envelope. No command string is involved.

Native calls: a method of a host component that has a native function (`wifi.get`, `wifi.set`, `wifi.tuning`, `wifi.set_tuning`; the `**C function:**` lines of `api/wifi.API.md`) is not sent to a module. The console looks the component up with `get_component`,
fills the request struct of the function from the fields of the line, calls the function in its table, and prints the output struct as the answer. The layout of the structs and the position of the function come from `native.h`
(`mesh-api-gen --out-console-native`, generated from the API.md and the Rust declaration), so adding a function to such an API adds no console code. A field the line does not name is zero, so `wifi.set` replaces the whole intent.

Commands: `status` (the host device_info map, including Wi-Fi, heap, slots and cached component status), `start NAME [TEXT]`, `stop NAME`, `reset`,
`wifi [ssid=NAME] [psk=SECRET] [start|stop]` (the network is a setting of this module, never in an image; an empty ssid is
the flash network), `loader start NAME` / `loader stop` (the upload server, which announces that the device wants the
package NAME), `help`.

Line syntax: `method`, `component.method` or `component method`, then `key=value` words. A key is a field name or
a number (`@3=5` and `3=5` are the same). Values: unsigned and negative integers, `true`/`false`, `hex:0a0b` for
bytes, anything else text; a double-quoted value may contain spaces. The declared type of a field wins over what a
value looks like (`name=123` stays text when `name` is a `string`).

## 1. `parse` — text line to request

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `line` | `string` | One line of text. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `component` | `uint` | |
| 2 | `method` | `uint` | |
| 5 | `fields` | `map` | The request fields, keyed by tag. |
| 12 | `local` | `string` | Present instead of the others when the line was answered here: `help`, or a syntax error. |

## 2. `render` — answer to text

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `component` | `uint` | The component of the request. |
| 2 | `method` | `uint` | The method of the request. |
| 6 | `result` | `map` | The answer (absent for an error). |
| 7 | `error` | `string` | The error text (absent on success). |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `text` | `string` | `key=value` words on one line; a record list one record per line; `ok` for an empty answer. |

The answers the console prints as lists, one record per line (`device_info` modules; the host's `modules_info`):

`Slot` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | |
| 2 | `tag` | `uint` | The component number of the module. |
| 3 | `installed` | `bool` | |
| 4 | `running` | `bool` | |
| 5 | `version` | `uint` | Of the installed package. |
| 6 | `last` | `int` | What the module returned the last time. |
| 7 | `runs` | `uint` | |
| 8 | `ms` | `uint` | Duration of the last run. |
| 9 | `stack_free` | `uint` | Worker stack high-water mark. |
| 10 | `check` | `int` | Result of the last start attempt's image check. |
| 11 | `reboot` | `bool` | The module stopped with memory it had not freed: it cannot be replaced until the device restarts. |

`Event` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `module` | `string` | |
| 2 | `id` | `uint` | The event id (`Hw::event`). |
| 3 | `text` | `string` | The text, or the bytes as hex. |


## 4. `dispatch` — binary envelope to binary answer

Dispatch one shared numeric CBOR envelope. This bridge executes console operations 5–10 and the generated native host methods (`wifi.get/set/tuning/set_tuning/monitor`, `power.*`). It does not invoke `parse`, render text, or synchronously call another module from the shared module worker. Other components return `ERR_UNSUPPORTED` (-2). In `fw/main`, installed module methods are registered directly in the DMesh handler registry and need no console wrapper.

The request itself is the envelope below, not a byte string or a command. The response retains its component, method and unsigned request ID. Result and error are mutually exclusive. Method 3 remains reserved for the existing internal text-line delivery path.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `component` | `uint` | Target component number; console is 20. |
| 2 | `method` | `uint` | Target method number. |
| 3 | `id` | `uint` | Correlation ID, echoed unchanged; defaults to zero. |
| 5 | `fields` | `map` | Target method request; defaults to an empty map. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `component` | `uint` | Request component. |
| 2 | `method` | `uint` | Request method. |
| 3 | `id` | `uint` | Request correlation ID. |
| 6 | `result` | `map` | Target method's unmodified successful CBOR result. |
| 7 | `error` | `int` | Negative ABI error code on failure. |

## 5. `status` — device snapshot

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `device` | `map` | Host device information, unchanged from `device_info`. |
| 2 | `wifi` | `map` | Host Wi-Fi snapshot. |
| 3 | `heap` | `map` | Heap information. |
| 4 | `modules` | `Slot[]` | Installed module slots. |
| 5 | `components` | `map` | Cached component status. |

## 6. `start` — start a module

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | Installed module name. |
| 2 | `request` | `bytes` | Opaque module start request; optional, defaults to empty. |

## 7. `stop` — queue module stop

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | Module name. Success means the stop was queued, not that shutdown completed. |

## 8. `reset` — request device reset

An empty request. Success means the reset request was accepted; obtain fresh application health after reboot to prove completion.

## 9. `heap` — heap snapshot or diagnostic request

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `kind` | `int` | Optional: 0 regions, 1 blocks, 2 tasks, 3 trace start, 4 trace dump, 5 trace stop, 6 CBOR block records over native module PPP. Absent returns the snapshot; present requests a diagnostic in the device log and returns an empty map. |

Kind 1 captures at most 1024 block descriptors into a temporary allocation, then prints after releasing allocator locks. The header identifies the snapshot buffer itself and whether the capture was truncated; truncation returns a size error after printing the captured blocks. The buffer is freed after output. Kind 6 sends the same snapshot as CBOR PPP records on port 1, followed by the ordinary correlated response. Each record has tag `0`: header `0` is `{0:0,1:count,2:seen,3:buffer_address,4:buffer_bytes,5:truncated,6:entry_total,7:entry_free}`; block `1` is `{0:1,1:heap_start,2:address,3:size,4:used}`; end `2` is `{0:2,1:count}`. The header includes the application-entry heap total/free, cached before application setup. Addresses and sizes are unsigned integers; `used` and `truncated` are booleans. This diagnostic stream requires native module PPP, such as generic C/Rust Main; ordinary FW control uses UART QUIC and does not carry this diagnostic stream.

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `free` | `uint` | Free heap bytes. |
| 2 | `largest` | `uint` | Largest free block. |
| 3 | `min_free` | `uint` | Minimum observed free heap. |
| 4 | `total` | `uint` | Total heap. |
| 5 | `internal_free` | `uint` | Free internal heap. |
| 6 | `internal_largest` | `uint` | Largest internal free block. |

## 10. `nv` — read or write a setting

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `key` | `string` | Setting name. |
| 2 | `value` | `bytes` | Optional: absent reads, present writes, empty restores the default. The setting's own validation applies. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `bytes` | Present on reads; writes return an empty map. |

Persistent Wi-Fi buffer settings are shared by C Main and FW Main:

| Key | Range | Default | Meaning |
|---|---|---:|---|
| `wifi.rx_static` | 2..64 | 10 | Fixed RX buffers allocated at Wi-Fi initialization. |
| `wifi.rx_dynamic` | 2..128 | 32 | Maximum dynamic RX buffers; effective value is at least the fixed count. |
| `wifi.rx_ba` | 2..64 | 6 | RX aggregation window, clamped to the effective buffer counts. |

Values are decimal ASCII **bytes**, e.g. `wifi.rx_static` with value `hex:3130`
stores 10. An empty byte string removes an override; an absent value reads the
stored override (error -4 when unset). Settings are committed to NVS and
apply at the next driver initialization; reboot to apply reliably. They do not
resize an active driver. Volatile `wifi.set_tuning` buffer fields take precedence
until reset. These defaults describe the current application builds; the SDK's
fixed RX storage must remain suitable for DMA.

Direct methods 5–10 return their result map on success or a signed negative ABI error scalar on failure (the existing module-call convention). `dispatch` places these failures in envelope field 7. Start/stop/reset return an empty successful map. Native Wi-Fi methods retain their own existing schemas and may be invoked through `dispatch`; text `wifi` remains available during migration. Existing DMesh status/settings methods keep their IDs and shapes and coexist with these console methods.


On the generic C runtime's PPP console port (1), a CBOR map is dispatched as this
numeric envelope and the response stays binary. Existing text records retain
their compatibility path. Binary port calls do not invoke `parse` or `render`.

`fw/main` also exposes the persistent boot setting `uart.baud` through `nv`
(decimal ASCII bytes, e.g. `hex:393231363030` for 921600). It applies on reboot,
so the reply is sent at the current rate. Volatile physical-rate exchange is a
C serial control operation, not QUIC or console policy: the shared host library
`uart-codec::host::UartPort::negotiate_baud_blocking` performs it before starting
a bearer. Native USB-JTAG ignores physical baud settings and rejects exchange.
