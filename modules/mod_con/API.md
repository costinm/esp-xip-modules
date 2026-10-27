# `console` API (20)

CBOR device control, and the home of the native components that no module hosts (Wi-Fi 10, ...). The module has no port. Methods are reached
as a numeric envelope in a 0x444d0002 datagram (the runtime parses it and sends the answer back the same way), or as direct calls from the application. The native components without a hosting module (`wifi.*`) are served from the native method table. Method IDs 1–4 are retired (4 was `dispatch`: an envelope as an argument; a module that holds an envelope sends it to `ADDR_ENVELOPE` and the runtime dispatches it).
Text lines, parsing, rendering and TCP console connections are not supported.
Method IDs 1–4 are retired and must not be reused.

Native Wi-Fi and IP methods use the generated component tables and keep their
existing schemas. Fields omitted from a native request are zero, so `wifi.set`
replaces the whole intent.

`Slot` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | |
| 2 | `tag` | `uint` | The component number of the module. |
| 3 | `installed` | `bool` | |
| 4 | `running` | `bool` | |
| 5 | `version` | `uint` | Of the active image: its manifest version (key 8, 0 without one); higher is newer. |
| 6 | `last` | `int` | What the module returned the last time. |
| 7 | `runs` | `uint` | |
| 8 | `ms` | `uint` | Duration of the last run. |
| 9 | `stack_free` | `uint` | Worker stack high-water mark. |
| 10 | `check` | `int` | Result of the last start attempt's image check. |
| 11 | `reboot` | `bool` | The module stopped with memory it had not freed: it cannot be replaced until the device restarts. |
| 12 | `page` | `uint` | The 64 KiB page of the module partition the active image sits on. |
| 13 | `shadowed` | `Shadowed[]` | The other installed images of this module name: lower versions (kept for recovery, the first pages to be reused) and duplicates. Empty when none. |

`Shadowed` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `page` | `uint` | The page of the shadowed image. |
| 2 | `version` | `uint` | Its version (manifest key 8). |
| 3 | `duplicate` | `bool` | The same version as the active image (the lowest page is the active one) instead of a lower one. |

## 5. `status` — device snapshot

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `device` | `map` | Host device information, unchanged from `device_info`. |
| 2 | `wifi` | `map` | Host Wi-Fi snapshot. |
| 3 | `heap` | `map` | Heap information. |
| 4 | `modules` | `Slot[]` | Installed module slots. |
| 5 | `components` | `map` | Cached component status. |
| 6 | `packets` | `map` | Global delivery queue depth, high-water mark, drops and pool usage; see host `device_info`. |

## 6. `start` — explicit lifecycle/debug activation

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | Installed module name. |
| 2 | `request` | `bytes` | Opaque module start request; optional, defaults to empty. |

Ordinary component commands activate their installed owner automatically. This
method is for explicit lifecycle/debug use and custom start parameters, not a
prerequisite for calling a component. Its success acknowledges queued startup;
ordinary component requests instead initialize the owner before dispatch and
return initialization errors.

## 7. `stop` — queue module stop

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | Module name. Success means the stop was queued, not that shutdown completed. |

## 8. `reset` — request device reset

An empty request. Success means the reset request was accepted; obtain fresh application health after reboot to prove completion.

Method 9 (heap) is retired; use system.heap, system.heap_dump or system.dump_tasks.

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

Persistent settings are shared by C Main and FW Main. `flash.auth` accepts the
ASCII bytes `strict` (default) or `unsigned`; an empty value restores strict.
The flash module reads it on start, including autostart after reboot. Changing
NV does not alter an active upload service: stop/start `flash` to apply it.
An explicit flash start request overrides NV for that activation.

Wi-Fi buffer settings:

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

## 11. `link.send` — DEBUG ONLY: send one packet on a link

Debugging aid. Sends one packet through the normal send path of the link at address `link` (the address the runtime reports for it, e.g. `0x40000001`
for the first native link registered after the serial one): a pool packet is filled with `data` and sent, and the link owns
it once it accepted. Errors are the link's: `ERR_BUSY` (-5, the link is still sending; nothing was kept, retry), `ERR_NOTFOUND` (-7, no such link),
`ERR_IO` (-3, for example the CoC channel is not open), `ERR_SIZE` (-8, more than a pool packet), `ERR_NOMEM`. It does not wait for the frame to leave.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `link` | `uint` | Link address. |
| 2 | `data` | `bytes` | The packet payload, 1 to 1500 bytes (the text `hex:...` is also accepted; it doubles the size). |
| 3 | `token` | `uint` | The destination token on the link; absent: 0 (broadcast or the only peer). |

Direct methods 5–8 and 10–11 return their result map on success or a signed negative ABI error scalar on failure (the existing module-call convention). An envelope request gets them in field 7. Start/stop/reset return an empty successful map. Native Wi-Fi methods retain their own existing schemas and are reached by envelope. Existing DMesh status/settings methods keep their IDs and shapes and coexist with these console methods.


The console has no PPP port. A request for component 20 (and for a native component that no module hosts) is an envelope in a 0x444d0002 long packet
(PPP port 0xf7 for `dmesh-cli`); the runtime parses it and delivers the method to this module as an ordinary call, then wraps the answer. Binary calls do not
invoke `parse` or `render`.

`fw/main` also exposes the persistent boot setting `uart.baud` through `nv`
(decimal ASCII bytes, e.g. `hex:393231363030` for 921600). It applies on reboot,
so the reply is sent at the current rate. Volatile physical-rate exchange is a
C serial control operation, not QUIC or console policy: the shared host library
`uart-codec::host::UartPort::negotiate_baud_blocking` performs it before starting
a bearer. Native USB-JTAG ignores physical baud settings and rejects exchange.
