# `espmod` module ABI

This file is the one definition of the interface between a **host** (an application, "main") and a **module**
(a small program that runs on it). `tools/gen_abi.py` reads it and writes `include/espmod.h` (C) and
`crates/espmod-abi/src/generated.rs` (Rust); `abi/espmod.lock` records the released slots. The layout follows
the `API.md` files of ssh-mesh and DMesh (numbered methods, tables for fields, prose and examples), without
depending on them. How to write a module: [docs/writing-modules.md](../docs/writing-modules.md).

<!-- espmod-abi: version=2 prefix=espmod released=false -->

## Model

A module is a set of **handlers**. It never waits, sleeps or loops: the host calls
`espmod_handle(host, state, call)` when something happens, the handler does its bounded work and returns, and the
host sleeps until the next alarm, interrupt or request. One host task runs every module's handlers, one at a
time, so a module costs memory for its image and its state block only.

```c
int espmod_handle(const espmod_host *host, void *state, const espmod_call *call);
```

* `state` is a zeroed block of `state_bytes` (image header) that the host keeps for the module. An image has no
  writable data, so everything the module remembers between calls lives there.
* `call->component` and `call->method` say what happened. They are a pair of integers chosen by **whoever
  created the event** and returned unchanged, as in the ssh-mesh envelope (keys `component`, `method`): the
  module picks them when it registers an alarm or an interrupt, main picks them when it posts a request. A
  module dispatches on them; component 0 is the host's own ([`HOST`](#host-component-0)).
* `call->data` / `call->len` is the payload (a request, usually one CBOR map keyed by field tags) and is valid
  during the call only. A handler answers a request with [`respond`](#4-respond--answer-the-current-request).
* Time budget: a handler returns within `CONFIG_ESPMOD_HANDLER_MS` (default 20 ms). `spi_transfer`,
  `i2c_transfer` and `adc_read` are bounded and synchronous. Anything longer (a radio transmission, a settle
  time) is started, and its end is an [alarm](#21-alarm_set--call-me-later-once-or-periodically) or an
  [interrupt](#12-irq_register--report-the-edges-of-a-pin).
* Return value: `0` keep running; [`RET_DONE`](#ret) the module is finished (the host destroys the instance after
  the call); a negative [result](#results) is logged and also destroys the instance.

Every call to the host takes `host->user` first; the table lists the other arguments. Check
`ESPMOD_HOST_HAS(host, call)` before using a call newer than the version a module was built against.

# Results

Success is 0 or positive, errors are negative.

| Name | Value | Meaning |
|---|---:|---|
| `OK` | 0 | Success. |
| `ERR_ARG` | -1 | Invalid argument. |
| `ERR_UNSUPPORTED` | -2 | The host does not implement this call (or the pin or bus is not allowed). |
| `ERR_IO` | -3 | The peripheral failed. |
| `ERR_TIMEOUT` | -4 | A bounded wait expired. |
| `ERR_BUSY` | -5 | The resource is in use (pin owned by another module, no free handle, previous request not yet dispatched). |
| `ERR_NOMEM` | -6 | Out of memory. |
| `ERR_NOTFOUND` | -7 | No such setting, handle or service. |
| `ERR_SIZE` | -8 | The buffer is too small (`*len` says how much is needed). |
| `ERR_LEAK` | -9 | A module ended holding packets (the host gets them back) or memory (`mem_alloc`: not recoverable, the slot needs a reboot to be replaced) it had not given back. The stop is reported as failed. |
| `ERR_AGAIN` | -10 | A nonblocking socket operation would block; wait for another readiness event. |

# Constants

## `HOST` (component 0)

Methods of component 0, which only the host uses. A module's own components are 1 and up.

| Name | Value | Description |
|---|---:|---|
| `COMPONENT` | 0 | The host's component number. |
| `START` | 1 | Once, after the instance is created. `data` is the start request (may be empty). |
| `STOP` | 2 | Main asks the module to end. The instance is destroyed after the call; release what is not released by that (a radio left transmitting). |
| `NETWORK` | 3 | Connectivity changed; `value` is 1 when main has a network path, else 0. Delivered to every running module (a handler that has nothing to do returns at once). |
| `WAKE` | 4 | A wake window opened. Only if the module asked with an alarm using `ALARM_WINDOW`. |

## `RET`

Return values of a handler.

| Name | Value | Description |
|---|---:|---|
| `CONTINUE` | 0 | Keep the instance. |
| `DONE` | 1 | The module is finished; destroy the instance. |

## `VALUE`

Value types of `emit`.

| Name | Value | Description |
|---|---:|---|
| `EMPTY` | 0 | No payload. |
| `U64` | 1 | 8 bytes, little endian. |
| `I64` | 2 | 8 bytes, little endian. |
| `BYTES` | 3 | Opaque bytes. |
| `TEXT` | 4 | UTF-8 text. |
| `CBOR` | 5 | One CBOR item. |

## `GPIO`

`gpio_config` modes and pulls.

| Name | Value | Description |
|---|---:|---|
| `INPUT` | 0 | |
| `OUTPUT` | 1 | |
| `INPUT_OUTPUT` | 2 | |
| `PULL_NONE` | 0 | |
| `PULL_UP` | 1 | |
| `PULL_DOWN` | 2 | |

## `IRQ`

`irq_register` edges.

| Name | Value | Description |
|---|---:|---|
| `FALLING` | 1 | |
| `RISING` | 2 | |
| `BOTH` | 3 | |

## `ALARM`

Flags of `alarm_set`.

| Name | Value | Description |
|---|---:|---|
| `EXACT` | 0 | Fire at the delay, waking the system if it has to. |
| `FLEXIBLE` | 1 | May fire anywhere in `[delay, delay + window]`; the host prefers the next wake window inside that range and fires at the end of the range at the latest. |
| `CONNECTED` | 2 | Do not fire while `connected()` is 0 (JobScheduler's network constraint). The alarm becomes due when connectivity returns and fires in the next wake window. |
| `WINDOW` | 4 | Also deliver `HOST_WAKE` whenever a wake window opens (a module that batches its work on windows). |

## `PKT`

Packets: the unit of every message between modules, and between a module and the application. A packet is a fixed
buffer from the host's pool with room in front of the payload for whoever frames it next (a network header, an envelope)
and behind it for a tag; a module writes only the payload window. The numbers are the contract of every host, whatever
its pool (the C pool of this repository, QUIC-lite's pool in a Rust host): see `docs/packets.md`.

| Name | Value | Description |
|---|---:|---|
| `HEADROOM` | 144 | Bytes in front of the payload window: 80 for link and relay headers, 13 short header, 3 frame header, 36 tagged-CBOR envelope, rounded up to 16. |
| `MAX` | 1000 | Largest payload a module can write or send. Larger records are refused with `ERR_SIZE`. |
| `SLOT_MIN` | 1204 | Smallest pool slot that holds `HEADROOM`, `MAX` and a 60-byte tail (AEAD tag, FCS). |

## `ADDR`

Where a packet is sent. A module is addressed by its component (the slot tag), which is also the `from` of what it sends.

| Name | Value | Description |
|---|---:|---|
| `MAIN` | 0 | The application (its packet hook). As `from`: the message came from the application, or the sender cannot be answered. |

## `FEAT`

Bits of `espmod_host.features`: what the host really implements.

| Name | Value | Description |
|---|---:|---|
| `SETTINGS` | 1 | `get_setting` / `set_setting`. |
| `SPI` | 2 | `spi_open` / `spi_transfer` / `spi_close`. |
| `SPI_READY` | 4 | The `ready_pin` option of `spi_open`. |
| `I2C` | 8 | `i2c_transfer`. |
| `ADC` | 16 | `adc_read`. |
| `RGBLED` | 32 | `rgbled_write`. |
| `SERVICE` | 64 | `call_service`. |
| `ALARM_FLEX` | 128 | `ALARM_FLEXIBLE` windows are real (the host has a sleepy mode); otherwise a flexible alarm fires at its delay. |
| `WIFI` | 256 | `wifi_start` / `wifi_stop` / `wifi_config` / `wifi_info` / `wifi_scan`. |
| `LOADER` | 512 | `loader_start` / `loader_stop`. |
| `SERIAL` | 1024 | `serial_open` / `serial_write` / `serial_close`. |
| `MODULES` | 4096 | `modules_info`, `module_start`, `module_stop`, `events_info`, `reset`. |
| `PACKETS` | 8192 | `pkt_*` and `send`, `call.pkt` / `call.from` / `call.id`. |
| `MEMORY` | 16384 | `mem_alloc` / `mem_free`. |
| `SOCKET` | 32768 | Nonblocking TCP sockets and packet writes. |
| `FLASH` | 65536 | Module-slot flash erase, pool-packet write, and short read; writes also require a signed manifest declaration. |

## `SOCKET_MODE`

| Name | Value | Description |
|---|---:|---|
| `DIRECT` | 0 | Readiness callback; the module calls nonblocking `socket_read` or `socket_write`. |
| `BUFFERED` | 1 | The runtime reads directly into pool packets and queues one packet per callback. |

## `SOCKET_EVENT`

| Name | Value | Description |
|---|---:|---|
| `OPEN` | 1 | Accepted socket; `call.id` is its handle, `call.pkt` and `call.len` are zero. |
| `READABLE` | 2 | Direct mode: `socket_read` may make progress. |
| `WRITABLE` | 3 | A direct write may progress or a refused packet write may be retried. |
| `DATA` | 4 | Buffered mode: `call.pkt` holds bytes from one TCP read; TCP message boundaries are not implied. |
| `EOF` | 5 | Peer closed its write half; distinct from the zero-length OPEN event. |
| `CLOSED` | 6 | Socket closed or errored; handle no longer valid after callback. |
| `WRITE_DONE` | 7 | A previously accepted packet has been fully submitted to the socket. |

## `DMOD`

Module image header.

| Name | Value | Description |
|---|---:|---|
| `MAGIC` | 0x444f4d44 | 'DMOD' little endian. |
| `HEADER_SIZE` | 64 | |
| `FLAG_FIXED_VMA` | 1 | Linked at code_vma/data_vma (Xtensa); otherwise position independent (RISC-V). |

# Types

## `spi_config` — argument of `spi_open`

Pins are GPIO numbers, -1 = unused.

| Field | Type | Description |
|---|---|---|
| `size` | `uint32_t` | `sizeof(espmod_spi_config)`, set by the caller; lets the struct grow. |
| `bus` | `int32_t` | SPI controller: 2 or 3 (3 does not exist on ESP32-C6, 2 is used). |
| `sck` | `int32_t` | |
| `miso` | `int32_t` | |
| `mosi` | `int32_t` | |
| `cs` | `int32_t` | |
| `hz` | `uint32_t` | Clock; 0 = 1 MHz. |
| `mode` | `int32_t` | SPI mode 0..3. |
| `ready_pin` | `int32_t` | Optional busy/ready line (SX126x BUSY), -1 = none. The host waits for `ready_level` before and after each transfer. |
| `ready_level` | `int32_t` | Level that means ready (0 for BUSY). |
| `ready_timeout_ms` | `uint32_t` | Longest wait for ready; 0 = 100. |

## `call` — what a handler is called with

| Field | Type | Description |
|---|---|---|
| `size` | `uint32_t` | `sizeof(espmod_call)`. |
| `component` | `int32_t` | Chosen by the creator of the event (0 = the host, see `HOST`). |
| `method` | `int32_t` | Chosen by the creator of the event. |
| `value` | `int32_t` | IRQ: the pin level. Alarm: how late it fired, in ms (saturated). `HOST_NETWORK`: 1 or 0. Request: 0. |
| `flags` | `uint32_t` | Bit 0: the call is a request that expects an answer (`respond`, or `send` to `from` with the same `id`). |
| `data` | `const uint8_t *` | Payload, valid during the call only. |
| `len` | `size_t` | Length of `data`. |
| `pkt` | `uint32_t` | The packet `data` lives in, lent for this call (0: none, as for alarms and interrupts). Keep it with `pkt_keep`, pass it on with `send`; otherwise it goes back to the pool when the handler returns. |
| `from` | `uint32_t` | Who sent it: a component (`ADDR_MAIN` = 0: the application). Answer with `send` to this address. |
| `reserved` | `uint32_t` | Zero; keeps `id` aligned the same on every target. |
| `id` | `uint64_t` | The sender's correlation number, returned in the answer. |

## `dmod_header` — first 64 bytes of a module image

Packed. The code follows directly.

| Field | Type | Description |
|---|---|---|
| `magic` | `uint32_t` | `DMOD_MAGIC`. |
| `abi_version` | `uint16_t` | `ABI_VERSION` the module was built for. |
| `header_size` | `uint16_t` | `DMOD_HEADER_SIZE`. |
| `entry_offset` | `uint32_t` | Offset of `espmod_handle` from the start of the image (includes the header). |
| `image_size` | `uint32_t` | Header plus code and read-only data. |
| `tag` | `uint16_t` | Module tag; must match the slot the image is installed in. |
| `reserved0` | `uint16_t` | Zero. |
| `code_vma` | `uint32_t` | FIXED_VMA: link address of the image start. |
| `data_vma` | `uint32_t` | FIXED_VMA: data-bus alias of the read-only data. |
| `stack_bytes` | `uint32_t` | Stack the deepest handler needs; the worker uses the largest of its modules. 0 = host default. |
| `required_features` | `uint32_t` | `FEAT_*` the module cannot run without. |
| `flags` | `uint32_t` | `DMOD_FLAG_*`. |
| `state_bytes` | `uint32_t` | Size of the zeroed state block passed to every call (at most 4096). |
| `manifest_offset` | `uint32_t` | Offset from the start of the image of the module's manifest (a multiple of 4, after the code); 0: none. |
| `manifest_size` | `uint32_t` | Its length. |
| `reserved` | `uint8_t[12]` | Zero. |

# Host calls

The host table, `espmod_host`. The method number is the slot: append only after a release. In the C and Rust
declarations the leading `void *user` argument is implied.

## 1. `log` — write one line to the host's log

TODO: replace with structured logging from dmesh after we add the 'packet pool': get the current out buffer, append CBOR tags and values instead of strings.

**Returns:** `int` — `OK`, or `ERR_SIZE` if the line was truncated.

The host truncates the line and adds no newline. A module's log is its only way to explain a failure.

### Arguments

| Name | Type | Description |
|---|---|---|
| `data` | `const uint8_t *` | Text. |
| `len` | `size_t` | Length. |

## 2. `now_ms` — monotonic milliseconds since boot

**Returns:** `uint64_t` — the time.

## 3. `respond` — answer the current request

TODO: not sure about this. The struct passed to the entrypint can take a return value, but 'data' is wrong - no copy if we can avoid it. Top priority is to port the packet pool from dmesh - so module can send messages. I don't think they need to 'respond', just send (like they do for log).

**Returns:** `int` — `OK`, `ERR_BUSY` if already answered, `ERR_ARG` if the call was not a request.

The answer to a call with `flags` bit 0. By convention one CBOR item (a map keyed by field tags, like the
request), delivered to whoever posted the request. No answer means an empty one.

### Arguments

| Name | Type | Description |
|---|---|---|
| `data` | `const uint8_t *` | The answer (copied). |
| `len` | `size_t` | Length. |

## 4. `emit` — send an event to the application

TODO: rename, like log and response it is a case of message send() - with component, method and data, and should use a borrowed packet from the pool for reduced copy. The modules receive and send messages - to other modules or remote.

**Returns:** `int` — `OK`, or an error.

The module's output channel besides `respond` and `log`; main receives `(module, event_id, value_type, value)`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `event_id` | `uint16_t` | Module-defined. |
| `value_type` | `uint8_t` | `VALUE_*`. |
| `value` | `const uint8_t *` | Payload (copied). |
| `len` | `size_t` | Length. |

## 5. `call_service` — call a service of the application by tag

TODO: would be great if we can avoid this or discourage it. First because of 'synchronous', also complicates upgrades and other things. If messages can be used - would be great.

**Returns:** `int` — `OK`, `ERR_NOTFOUND` when nothing answers.

The call is synchronous and must be short; `timeout_ms` bounds it. For anything slow ask the service to
answer later through a request to this module.

### Arguments

| Name | Type | Description |
|---|---|---|
| `tag` | `uint16_t` | The service. |
| `request` | `const uint8_t *` | Request. |
| `request_len` | `size_t` | Length. |
| `response` | `uint8_t *` | Buffer for the answer. |
| `response_capacity` | `size_t` | Its size. |
| `response_len` | `size_t *` | Answer length. |
| `timeout_ms` | `uint32_t` | Longest wait. |

## 6. `get_setting` — read a persistent setting of this module

**Returns:** `int` — `OK`, `ERR_NOTFOUND` if absent, `ERR_SIZE` if the buffer is small.

Own namespace; the key is up to 15 bytes, the value up to 256.

### Arguments

| Name | Type | Description |
|---|---|---|
| `key` | `const uint8_t *` | Name. |
| `key_len` | `size_t` | Length. |
| `value` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |
| `value_len` | `size_t *` | Value length. |

## 7. `set_setting` — write a persistent setting of this module

**Returns:** `int` — `OK` or an error.

An empty value erases the setting.

### Arguments

| Name | Type | Description |
|---|---|---|
| `key` | `const uint8_t *` | Name. |
| `key_len` | `size_t` | Length. |
| `value` | `const uint8_t *` | Value. |
| `value_len` | `size_t` | Length. |

## 8. `gpio_config` — configure a pin

**Returns:** `int` — `OK`, `ERR_BUSY` if another module owns it, `ERR_UNSUPPORTED` if main reserved it.

Also sets the initial level of an output. The pin belongs to the module until the instance is destroyed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | GPIO number. |
| `mode` | `int` | `GPIO_*`. |
| `pull` | `int` | `GPIO_PULL_*`. |
| `level` | `int` | Initial output level. |

## 9. `gpio_read` — read a pin

**Returns:** `int` — level 0/1, or a negative error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | A pin configured by this module. |

## 10. `gpio_write` — set an output level

**Returns:** `int` — `OK` or an error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | A pin configured by this module. |
| `level` | `int` | 0 or 1. |

## 12. `irq_unregister` — stop reporting a pin

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The pin. |

## 11. `irq_register` — report the edges of a pin

**Returns:** `int` — `OK` or an error.

An edge becomes a call `(component, method)` with `value` = the pin level. The interrupt itself never runs
module code; edges that arrive while the queue is full are counted and dropped.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The pin. |
| `edge` | `int` | `IRQ_*`. |
| `component` | `int` | Returned in the call. |
| `method` | `int` | Returned in the call. |

## 13. `irq_enable` — mask or unmask a registered pin

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The pin. |
| `enabled` | `int` | 1 unmask, 0 mask. |

## 14. `spi_open` — open a full-duplex SPI device

**Returns:** `int` — a handle (>= 0) or a negative error.

The first open of a controller initialises it with the pins of the config; later opens must use the same
pins. A controller that main initialised is shared as is.

### Arguments

| Name | Type | Description |
|---|---|---|
| `config` | `const espmod_spi_config *` | See `spi_config`. |

## 15. `spi_transfer` — one chip-select cycle

**Returns:** `int` — `OK`, `ERR_BUSY` when `ready_pin` never became ready, `ERR_IO`.

`tx` and `rx` are both `len` bytes (`rx` may be NULL). Bounded; waits for `ready_pin` when configured.

### Arguments

| Name | Type | Description |
|---|---|---|
| `handle` | `int` | From `spi_open`. |
| `tx` | `const uint8_t *` | Bytes to send. |
| `rx` | `uint8_t *` | Bytes received, or NULL. |
| `len` | `size_t` | Length. |

## 20. `alarm_set` — call me later, once or periodically

**Returns:** `int` — an alarm id (>= 0) or a negative error (`ERR_NOMEM` when the module has 8 alarms).

At `delay_ms` from now the host calls the handler with `(component, method)` and `value` = lateness in
milliseconds. `interval_ms` > 0 makes it periodic (a poll): it is re-armed from its scheduled time, not from
the time it ran. A timeout is a one-shot alarm. With `ALARM_FLEXIBLE` the call may be delayed up to
`window_ms` so that it runs together with other work in the host's next wake window. All alarms of an
instance are cancelled when it is destroyed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `delay_ms` | `uint32_t` | First fire, from now. |
| `window_ms` | `uint32_t` | Flexibility; used with `ALARM_FLEXIBLE`. |
| `interval_ms` | `uint32_t` | 0 = once; otherwise the period. |
| `flags` | `uint32_t` | `ALARM_*`. |
| `component` | `int` | Returned in the call. |
| `method` | `int` | Returned in the call. |

### Example: poll a battery every minute, whenever convenient

```c
host->alarm_set(host->user, 60000, 30000, 60000, ESPMOD_ALARM_FLEXIBLE, MY_BATTERY, POLL);
```

## 21. `alarm_cancel` — cancel an alarm

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `alarm` | `int` | An id from `alarm_set`. |

## 22. `connected` — does main have a network path

**Returns:** `int` — 1 yes, 0 no.

Cheap. Use `ALARM_CONNECTED` to defer work until it is true.

## 16. `spi_close` — release an SPI device

**Returns:** `int` — `OK`, `ERR_ARG`.

Also done when the instance is destroyed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `handle` | `int` | From `spi_open`. |

## 17. `i2c_transfer` — write then read on an I2C bus

**Returns:** `int` — `OK` or an error.

Either direction may be empty. The pins are held for the call only.

### Arguments

| Name | Type | Description |
|---|---|---|
| `port` | `int` | Controller. |
| `sda` | `int` | Pin. |
| `scl` | `int` | Pin. |
| `frequency` | `uint32_t` | Hz. |
| `address` | `uint8_t` | 7-bit address. |
| `tx` | `const uint8_t *` | Bytes to write. |
| `tx_len` | `size_t` | Length. |
| `rx` | `uint8_t *` | Buffer for the read. |
| `rx_len` | `size_t` | Length. |
| `timeout_ms` | `uint32_t` | Longest wait. |

## 18. `adc_read` — one-shot 12-bit reading

**Returns:** `int` — `OK` or an error.

`unit` and `channel` may be NULL.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | GPIO number. |
| `ref_mv` | `uint32_t` | Reference for the millivolt value. |
| `raw` | `int *` | Raw reading. |
| `mv` | `uint32_t *` | Millivolts. |
| `unit` | `int *` | ADC unit used. |
| `channel` | `int *` | ADC channel used. |

## 19. `rgbled_write` — set a WS2812-style LED

**Returns:** `int` — `OK` or an error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The data pin. |
| `red` | `uint8_t` | 0..255. |
| `green` | `uint8_t` | 0..255. |
| `blue` | `uint8_t` | 0..255. |

## 23. `wifi_start` — join a Wi-Fi network

**Returns:** `int` — `OK` (joining continues in the background), `ERR_UNSUPPORTED` without Wi-Fi, `ERR_BUSY` if already started.

Joins the named network and keeps rejoining until `wifi_stop`. The call returns at once (joining takes seconds); poll
`wifi_info`. The caller keeps the name and passphrase in its own settings, never in an image: an empty `ssid` means the network set with
`wifi_config` if a scan sees it, else the flash network (`DIRECT-xy-flash`, hosted by the flashing phone); an empty `psk` means that network's public passphrase. Nothing is on
the network until a module asks.

### Arguments

| Name | Type | Description |
|---|---|---|
| `ssid` | `const uint8_t *` | Network name, or empty. |
| `ssid_len` | `size_t` | Length (at most 32). |
| `psk` | `const uint8_t *` | Passphrase, or empty. |
| `psk_len` | `size_t` | Length (at most 63). |

## 24. `wifi_stop` — leave the network and switch the radio off

**Returns:** `int` — `OK`; the radio is released in the background.

## 25. `wifi_info` — state of the Wi-Fi

**Returns:** `int` — the length of the answer, or a negative error (`ERR_SIZE`: the buffer is too small).

One CBOR map: `{1: state, 2: ssid, 3: ip, 4: rssi, 5: saved ssid, 6: psk set}`: `state` is 0 off, 1 joining, 2 up; 2 to 4 are present once
up (`ip` is the IPv4 address as text, `rssi` is signed dBm); 5 and 6 are present when a network is configured (`wifi_config`).

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 26. `wifi_config` — the network this device uses when it is not told one

**Returns:** `int` — `OK`, `ERR_UNSUPPORTED` without Wi-Fi, `ERR_ARG` for a name over 32 or a passphrase over 63 bytes, `ERR_IO` if it cannot be stored.

Stores the device's own network (SSID and passphrase, in the loader's settings, not in any module's and never in an image). An
empty `ssid` clears it. It is **optional**: `wifi_start` with an empty `ssid`, and the loader that installs `main` and modules,
join this network when a scan sees it and use the flash network (`DIRECT-xy-flash`) when it does not or when none is configured, so
a device installs signed code with no network of the user's. `wifi_info` reports it (keys 5 and 6).

### Arguments

| Name | Type | Description |
|---|---|---|
| `ssid` | `const uint8_t *` | Network name, or empty to clear. |
| `ssid_len` | `size_t` | Length (at most 32). |
| `psk` | `const uint8_t *` | Passphrase, or empty (open network). |
| `psk_len` | `size_t` | Length (at most 63). |

## 27. `loader_start` — accept a signed package called `name`

**Returns:** `int` — `OK`, `ERR_UNSUPPORTED` without a loader, `ERR_BUSY` if already started, `ERR_ARG` for a bad name.

Opens the upload server of esp-wifi-loader (needs `wifi_start` to be up) and announces on the network, by UDP multicast
every few seconds, that this device asks for the package `name` (a module name, or `main`); a flasher that sees the
announce sends it, signed. How the flasher learns the name may change (the loader could ask the flash station instead);
the call stays. The host closes nothing by itself: stop it with `loader_stop`, or set an alarm.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Package name. |
| `name_len` | `size_t` | Length (at most 32). |

## 28. `loader_stop` — close the upload server

**Returns:** `int` — `OK`.

## 29. `serial_open` — receive the lines typed on the board's serial port

**Returns:** `int` — `OK`, `ERR_UNSUPPORTED`, `ERR_BUSY` if another module has the port, `ERR_IO` if there is no port.

The host owns the port: it picks USB-Serial/JTAG or UART0, runs the driver tuned for the board, edits the line (backspace)
and starts and stops all of it. Each line becomes a packet `(component, method)` to this module (`call.pkt`, `from` = `ADDR_MAIN`, `id` 0), `data` the line without
a newline. Nothing is answered: the module writes what it has to say with `serial_write`, whenever it has it. The port is
released by `serial_close` or when the module ends.

### Arguments

| Name | Type | Description |
|---|---|---|
| `component` | `int` | Returned in the call. |
| `method` | `int` | Returned in the call. |

## 30. `serial_write` — write text to the serial port

**Returns:** `int` — the bytes taken, or a negative error (`ERR_NOTFOUND` when this module does not have the port).

For output that is not the answer to a line.

### Arguments

| Name | Type | Description |
|---|---|---|
| `data` | `const uint8_t *` | Text. |
| `len` | `size_t` | Length. |

## 31. `serial_close` — give the serial port back

**Returns:** `int` — `OK`; the driver is released in the background.

## 32. `modules_info` — every module slot

**Returns:** `int` — the length of the answer, or a negative error.

One CBOR array with a map per slot, keyed as the `slot` record of `modules/mod_console/API.md`: name, tag, installed, running,
version, last result, runs, duration of the last run, stack high-water mark, result of the last image check.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 33. `module_start` — start the module installed in a slot

**Returns:** `int` — `OK` (the module starts in the background), `ERR_NOTFOUND` for an unknown name, or the loader's error.

`request` is what the module receives with `HOST_START`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Slot name. |
| `name_len` | `size_t` | Length. |
| `request` | `const uint8_t *` | Start request, may be empty. |
| `request_len` | `size_t` | Length. |

## 34. `module_stop` — ask a module to end

**Returns:** `int` — `OK` (it ends in the background; poll `modules_info`), `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Slot name. |
| `name_len` | `size_t` | Length. |

## 35. `events_info` — the last events modules emitted

**Returns:** `int` — the length of the answer.

One CBOR array, oldest first, of maps keyed as the `event` record of `modules/mod_console/API.md`: module, event id, and the
value as text (a text value as it is, any other as hex). The host keeps the last 16.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 36. `reset` — restart the device

**Returns:** `int` — `OK`; the restart follows in 300 ms.

## 37. `pkt_alloc` — take an empty packet

**Returns:** `int` — `OK` and the handle in `*pkt`; `ERR_NOMEM` when the pool is down to the network's reserve; `ERR_BUSY` when the module already holds its quota.

The packet is **held** by this module: it counts against a small fixed quota (`CONFIG_ESPMOD_PKT_HELD`, default 4) and is
returned to the pool when the module sends or frees it, or ends. The handle is an opaque number that is never 0 and
stops being valid when the packet leaves the module (a stale handle gets `ERR_ARG`, not someone else's packet).

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t *` | The new handle. |

## 38. `pkt_buf` — the payload window of a packet

**Returns:** `int` — `OK`; `ERR_ARG` for a handle that is not valid; `ERR_BUSY` when the packet is shared (a received packet that something else still reads cannot be written).

`*ptr` points at the payload window, `*max` is how many bytes may be written (at most `PKT_MAX`). For a packet received
in a call the window is the received payload.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t` | The handle. |
| `ptr` | `uint8_t **` | The window. |
| `max` | `size_t *` | Its size. |

## 39. `pkt_keep` — keep the packet of this call after the handler returns

**Returns:** `int` — `OK` (the packet is now held, like one from `pkt_alloc`), `ERR_ARG`, `ERR_BUSY` when the module holds its quota.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t` | `call.pkt`. |

## 40. `pkt_free` — give a packet back

**Returns:** `int` — `OK` or `ERR_ARG`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t` | A held packet. |

## 41. `send` — hand a packet to a module or to the application

**Returns:** `int` — `OK`; `ERR_NOTFOUND` (no running module has that component), `ERR_BUSY` (its inbox is full), `ERR_SIZE`, `ERR_ARG`.

The packet moves: **the handle is consumed on success and on failure**, so there is never a doubt who owns it. The first
`len` bytes of the payload window are the message. The receiver gets a call `(component, method)` with `call.pkt`,
`call.from` = this module's component and `call.id` = `id`. A non-zero `id` asks for an answer, and the receiver sees
`flags` bit 0: it answers with a `send` to that `from` with the same `id` (or with `respond`, which the host turns into the
same message). The answer is an ordinary message, and so is every other one the module sends the asker: any number of them,
any time, the same `id` or another. Nobody waits for an answer, and a module with nothing to say says nothing.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t` | A held packet, or the packet of this call. |
| `len` | `size_t` | Message length, at most `PKT_MAX`. |
| `dst` | `uint32_t` | `ADDR_MAIN` or a module's component. |
| `component` | `int` | Returned in the receiver's call. |
| `method` | `int` | Returned in the receiver's call. |
| `id` | `uint64_t` | Correlation number, returned in the call. |

## 42. `pkt_info` — the geometry of this host's packets

**Returns:** `int` — `OK`.

For a host whose slots are larger than `SLOT_MIN`; `*headroom` is at least `PKT_HEADROOM` and `*max` at most what the
payload window offers.

### Arguments

| Name | Type | Description |
|---|---|---|
| `headroom` | `uint32_t *` | Bytes in front of the window. |
| `max` | `uint32_t *` | Largest payload. |

## 43. `mem_alloc` — memory that outlives a call

**Returns:** `void *` — NULL when the module's quota (`CONFIG_ESPMOD_MEM_QUOTA` bytes) or the heap is exhausted.

The heap is managed by balance: the host adds up the bytes a module takes and gives back. A message belongs in a packet
(`pkt_alloc`); this is for what a module keeps between calls and does not fit in its state block (a reassembly buffer, a
queue of frames). **A module frees everything it allocated before it stops.** If the balance is not zero when it ends, the
stop is reported as failed (`ERR_LEAK`, bytes in `espmod_stats_t.mem_leaked`) and the module **cannot be replaced live**:
the host does not know which blocks are left, so the slot is marked `needs_reboot` and an install into it is refused until the
device restarts. A leak therefore shows up in the first test rather than as a slow loss of memory in the field. The block is
aligned for any type.

### Arguments

| Name | Type | Description |
|---|---|---|
| `size` | `size_t` | Bytes. |

## 44. `mem_free` — give a block back

**Returns:** `int` — `OK`, or `ERR_ARG` for a pointer this module did not allocate or already freed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `ptr` | `void *` | From `mem_alloc`. |

## 45. `module_manifest` — what an installed module says about itself

**Returns:** `int` — the length of the manifest (0: the module has none), `ERR_NOTFOUND` for an unknown name, `ERR_SIZE` when the buffer is too small.

A module image carries a **manifest** at the offset its header gives (`manifest_offset`, `manifest_size`), read from flash
without starting the module, so it is available for an installed module that is not running and for one installed after the
console was built. It is the module's `API.md` as a compact CBOR catalog, written by `tools/mkmod.py`: the module's name and
component, its methods (id, name, and which request and response field list each uses), the field lists (tag, name, kind
and, for a record, its type) and the record types. It is the equivalent of an MCP `tools.json` and carries what a text or
JSON front end needs to turn `NAME.METHOD key=value` into the module's request without any ID compiled into it: the
component of a module is found by name in `modules_info`, everything below it in the manifest. The module's own code never
reads it. (Format: `tools/mkmod.py`, `manifest_of_api`.)

Manifest key 7 is an optional array of module slot indices the image may erase or write. The packer sets it with
`--flash-slots=INDEX[,INDEX...]` and requires `FEAT_FLASH`; this declaration must be covered by the verified signed
install package. A host must never grant write access based only on `required_features` or an unverified manifest.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Slot name. |
| `name_len` | `size_t` | Length. |
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 46. `wifi_scan` — the networks in range

**Returns:** `int` — the length of the answer, `ERR_BUSY` while the scan is running, `ERR_UNSUPPORTED` without Wi-Fi, `ERR_IO` if the driver refused, `ERR_SIZE` when the buffer is too small.

Never blocks: a scan takes seconds and a handler has a budget of milliseconds. The first call starts the scan and returns `ERR_BUSY`;
poll with an alarm (about every second) and the call that finds it done returns the answer, once. The radio is switched on for the scan
and off again when it was off; when a join is up it is scanned from the running station (`ERR_BUSY` also while a join is under way). The
answer is one CBOR array, strongest first, of maps `{1: ssid, 2: bssid, 3: rssi, 4: channel, 5: auth}`: `ssid` and `bssid` are byte strings (the
name is not always text), `rssi` is signed dBm, `auth` is 0 open, 1 WEP, 2 WPA, 3 WPA2, 4 WPA/WPA2, 5 enterprise, 6 WPA3 (others: the
driver's number). A network that is in range but not listed was missed (a scan can); scan again.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 47. `heap_info` — free memory of the device

**Returns:** `int` — the length of the answer, `ERR_SIZE` when the buffer is too small.

RAM is the scarce resource on these chips, so the host reports it to any module that asks (the console's `heap`). One CBOR map of
byte counts: `{1: free, 2: largest block, 3: lowest free since boot, 4: total, 5: free internal RAM, 6: largest internal block}`.
"Largest block" is the biggest single allocation that would succeed now; a heap can have plenty free and still refuse a
request (fragmentation). Free memory is **tracked**, not only reported: a build that logs it on a change or at boot has the
history, and the lowest value (3) shows the worst moment since boot, which is the number to size things by.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 48. `nv` — a device setting by name

**Returns:** `int` — reading: the length of the text put in `out`; writing: `OK`. `ERR_NOTFOUND` for a name the device does not have (or
that is not set), `ERR_ARG` for a bad value, `ERR_IO` if it cannot be stored, `ERR_SIZE` when `out` is too small, `ERR_UNSUPPORTED`
when the application offers none.

The settings of the *device*, kept in its NVS by the application and the loader, as text: `wifi.rx_static`, `wifi.rx_dynamic`,
`wifi.tx_dynamic`, `wifi.rx_ba` (the Wi-Fi driver's buffer counts, which are RAM) from the loader, and whatever else the application
lists (DMesh: `uart.mode`, `quic.pool`, ...). The host knows no names: it passes them on, and the application answers for its own. This is for a
console or a tool; a module's own settings are `get_setting` / `set_setting`. With `value` NULL it reads; with a value it writes
(an empty value restores the default). Most settings apply at the next start of the thing they configure, or after a reboot.

### Arguments

| Name | Type | Description |
|---|---|---|
| `key` | `const uint8_t *` | Setting name. |
| `key_len` | `size_t` | Its length. |
| `value` | `const uint8_t *` | New value, or NULL to read. |
| `value_len` | `size_t` | Its length. |
| `out` | `uint8_t *` | Buffer for the value that is read. |
| `capacity` | `size_t` | Its size. |

## 49. `heap_dump` — print where the memory is, to the device log

**Returns:** `int` — `OK`, `ERR_ARG` for an unknown `kind`, `ERR_UNSUPPORTED` when the build lacks what the kind needs.

`heap_info` says how much is free; this says what is there. The output goes to the **device log** (the port's text output, never
into a reply), because it is long and for a person. `kind`: 0 the heap regions (size, free, largest block, block counts), 1 every block
of the heap (address, size, free or used: the picture of fragmentation), 2 the tasks (name, priority, stack left; needs FreeRTOS
trace facility), 3 start recording who allocates (`CONFIG_HEAP_TRACING_STANDALONE`; slows allocation, for a bench), 4 print what is
still allocated with the call sites (the owners: this is the one that answers "who holds the RAM"), 5 stop recording.

### Arguments

| Name | Type | Description |
|---|---|---|
| `kind` | `int` | What to print (above). |

## 50. `tcp_accept` — register a nonblocking TCP listener

**Returns:** `int` — `OK`; `ERR_ARG`, `ERR_BUSY`, `ERR_IO`, `ERR_UNSUPPORTED`.

The host owns the listener and accepted sockets. It calls `(component, method)` on the module worker for socket events; `call.value` is `SOCKET_EVENT_*` and `call.id` is the opaque socket handle. The first accepted-socket event is `OPEN` with zero data length. A handle belongs to this instance and is closed when it stops. The listener can be closed with `socket_close`. `SOCKET_MODE_DIRECT` reports readiness only. `SOCKET_MODE_BUFFERED` reads into pool packets and reports `DATA`; the module must release or keep each packet. No callback runs on the poll task.

### Arguments

| Name | Type | Description |
|---|---|---|
| `port` | `uint16_t` | TCP listen port. |
| `mode` | `uint32_t` | `SOCKET_MODE_DIRECT` or `SOCKET_MODE_BUFFERED`. |
| `component` | `int32_t` | Component of socket events. |
| `method` | `int32_t` | Method of socket events. |
| `listener` | `uint32_t *` | Receives an opaque listener handle. |

## 51. `socket_read` — nonblocking read into module memory or a packet span

**Returns:** `int` — bytes read, 0 at EOF, `ERR_AGAIN` if no bytes are ready; `ERR_ARG`, `ERR_IO`.

Only for direct-mode accepted sockets. One call may read fewer than `len` bytes. The buffer is borrowed for this call only. A packet span may be passed as `out` while the module owns its mutable lease.

### Arguments

| Name | Type | Description |
|---|---|---|
| `socket` | `uint32_t` | Accepted socket handle. |
| `out` | `uint8_t *` | Destination. |
| `len` | `size_t` | Maximum bytes to read. |

## 52. `socket_write` — nonblocking POSIX-style write

**Returns:** `int` — bytes written, `ERR_AGAIN` if no bytes were written; `ERR_ARG`, `ERR_IO`.

The host does not keep `data` after return. A short write leaves the rest with the caller. The module requests another `WRITABLE` event by returning to the worker; it never waits in the handler.

### Arguments

| Name | Type | Description |
|---|---|---|
| `socket` | `uint32_t` | Accepted socket handle. |
| `data` | `const uint8_t *` | Bytes to write. |
| `len` | `size_t` | Byte count. |

## 53. `socket_close` — close a listener or accepted socket

**Returns:** `int` — `OK`, `ERR_ARG`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `socket` | `uint32_t` | Handle to close. |

## 54. `socket_peer` — peer address metadata

**Returns:** `int` — `OK`, `ERR_ARG`, `ERR_IO`.

The output is an IPv6 address (IPv4 is represented as IPv4-mapped IPv6), port in host byte order, and IPv6 scope. The socket handle is not a raw lwIP descriptor.

### Arguments

| Name | Type | Description |
|---|---|---|
| `socket` | `uint32_t` | Accepted socket handle. |
| `address` | `uint8_t *` | Exactly 16 writable bytes. |
| `port` | `uint16_t *` | Peer port. |
| `scope` | `uint32_t *` | IPv6 interface scope. |

## 55. `pkt_span` — full mutable pool slot for framing

**Returns:** `int` — `OK`, `ERR_ARG`, `ERR_BUSY` when shared.

Returns the full slot and capacity while the packet is uniquely owned. The caller chooses a valid prefix, payload and suffix range; `socket_write_pkt` validates the selected range. Pointers do not survive packet release or transfer.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pkt` | `uint32_t` | Held packet. |
| `base` | `uint8_t **` | Receives the slot base pointer. |
| `capacity` | `size_t *` | Receives its byte capacity. |

## 56. `socket_write_pkt` — transfer a pool packet for runtime-buffered output

**Returns:** `int` — `OK`, `ERR_BUSY`, `ERR_ARG`, `ERR_SIZE`, `ERR_IO`.

On `OK` the runtime owns the packet until the whole selected range has been submitted, then emits `WRITE_DONE`. On every error the caller retains the exact packet. TCP may write part of it; the runtime retains the remaining range. The runtime admits at most one pending packet per socket. This differs from `send`, which consumes on failure.

### Arguments

| Name | Type | Description |
|---|---|---|
| `socket` | `uint32_t` | Accepted socket handle. |
| `pkt` | `uint32_t` | Held packet. |
| `start` | `uint32_t` | First byte within the slot. |
| `len` | `uint32_t` | Bytes to submit. |

## 57. `flash_erase` — enqueue a module-slot erase

**Returns:** `int` — `OK` when queued; `ERR_BUSY`, `ERR_ARG`, `ERR_IO`, `ERR_UNSUPPORTED`.

`slot` is an index in the host's module slot table, and `offset` is relative to that slot. The host resolves it through the configured data partition; physical addresses are never supplied by a module. Unknown slots, zero length, overflow, a range beyond the slot or data partition, and non-4096-byte-aligned erase ranges return `ERR_ARG`. Main, Stage2, the loader, NVS, and bytes outside a configured module slot are inaccessible. The signed image manifest must list the target slot in key 7 (`flash_slots`); `FEAT_FLASH` only announces host availability. A missing or untrusted declaration returns `ERR_ARG`. The host rejects an erase of a mapped/running XIP target with `ERR_BUSY`; the target must be stopped and unmapped first.

Erase runs on a flash worker, never on the module or poll worker. At most one flash job per caller and one device-wide flash job may be pending. On completion, a still-running caller receives `(component, method)` with `call.id = id`, `call.value = OK` or a negative error, and no payload. A caller stop cancels a queued job. If the worker has started it, stop waits for that job to finish before destroying the instance or reusing its slot; no callback is delivered after stop. The host rechecks the target's XIP mapping immediately before erase. A stopped caller cannot submit another job.

### Arguments

| Name | Type | Description |
|---|---|---|
| `slot` | `uint32_t` | Target index in the configured module slot table. |
| `offset` | `uint32_t` | Byte offset within that slot. |
| `len` | `uint32_t` | Erase byte count. |
| `component` | `int32_t` | Completion component. |
| `method` | `int32_t` | Completion method. |
| `id` | `uint64_t` | Caller correlation number. |

## 58. `flash_write` — transfer a pool packet for a module-slot write

**Returns:** `int` — `OK` when queued; `ERR_BUSY`, `ERR_ARG`, `ERR_SIZE`, `ERR_IO`, `ERR_UNSUPPORTED`.

The same slot range and signed-manifest checks as `flash_erase` apply. `start + len` must fit the uniquely held pool slot and `len` must be nonzero; invalid ranges return `ERR_ARG`. The host writes directly from the packet's selected span. On `OK` it takes ownership of the exact packet until the write completes or is canceled, then releases it; it allocates no separate 4096-byte job buffer. On any error the caller retains ownership. The pool slot capacity is the maximum job payload, so larger upload blocks are split by the module. Completion and stop follow `flash_erase`; no callback is delivered after the caller stops. The host rejects writes to a mapped/running target with `ERR_BUSY`, including a caller's own XIP slot.

### Arguments

| Name | Type | Description |
|---|---|---|
| `slot` | `uint32_t` | Target index in the configured module slot table. |
| `offset` | `uint32_t` | Byte offset within that slot. |
| `pkt` | `uint32_t` | Uniquely held pool packet; transferred only on `OK`. |
| `start` | `uint32_t` | First byte within the pool slot. |
| `len` | `uint32_t` | Byte count, at most the pool slot capacity. |
| `component` | `int32_t` | Completion component. |
| `method` | `int32_t` | Completion method. |
| `id` | `uint64_t` | Caller correlation number. |

## 59. `flash_read` — read a module slot into module memory

**Returns:** `int` — bytes read; `ERR_ARG`, `ERR_IO`, `ERR_SIZE`, `ERR_UNSUPPORTED`.

A short read is synchronous and limited to 4096 bytes. `slot`, `offset` and `len` must designate an existing configured module slot and fit both that slot and the data partition; invalid ranges return `ERR_ARG`. This call cannot read Main, Stage2, the loader, NVS, or the state log outside module slots. A module should split larger reads over callbacks. Reads do not require the manifest's write permission.

### Arguments

| Name | Type | Description |
|---|---|---|
| `slot` | `uint32_t` | Source index in the configured module slot table. |
| `offset` | `uint32_t` | Byte offset within that slot. |
| `out` | `uint8_t *` | Destination. |
| `len` | `size_t` | At most 4096 bytes. |


# Helpers

Not host calls: header-only code in `include/espmod_cbor.h` (C) and `espmod-abi`'s `cbor` module (Rust), for
modules and hosts alike.

* Encoder: `em_cbor_uint`, `em_cbor_int`, `em_cbor_bytes`, `em_cbor_text`, `em_cbor_map`, `em_cbor_array`,
  `em_cbor_bool` write to a bounded buffer and report overflow once, at the end.
* Decoder: `em_cbor_next` reads one item (type, integer, or a pointer and length for bytes and text) and
  `em_cbor_skip` steps over a whole item; `em_cbor_field` finds the value of an unsigned-integer key in a map.
* Only definite lengths, integers up to 64 bits, byte and text strings, arrays, maps and booleans; floats and
  tags are skipped but not decoded.
