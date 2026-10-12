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
| `WIFI` | 256 | `wifi_start` / `wifi_stop` / `wifi_info`. |
| `LOADER` | 512 | `loader_start` / `loader_stop`. |
| `SERIAL` | 1024 | `serial_open` / `serial_write` / `serial_close`. |
| `CALL` | 2048 | `call`. |
| `MODULES` | 4096 | `modules_info`, `module_start`, `module_stop`, `events_info`, `reset`. |

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
| `flags` | `uint32_t` | Bit 0: the call is a request that expects `respond`. |
| `data` | `const uint8_t *` | Payload, valid during the call only. |
| `len` | `size_t` | Length of `data`. |

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
| `reserved` | `uint8_t[20]` | Zero. |

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

## 2. `alloc` — scratch memory for this call

TODO: we can also add proper alloc/free - along with borrowing packets from the pool. Modules must free everything before 'stop' and have limits, but it is ok to have memory that survives. 
TODO: we can document the available stack - not clear if we need a separate short-lived memory.

**Returns:** `void *` — NULL when out of memory.

**Tier:** internal

Bump allocation owned by the host; there is no free: everything is released when the handler returns. Use the
state block for anything that must survive the call.

### Arguments

| Name | Type | Description |
|---|---|---|
| `size` | `size_t` | Bytes. |
| `align` | `size_t` | Power of two. |

## 3. `now_ms` — monotonic milliseconds since boot

**Returns:** `uint64_t` — the time.

## 4. `respond` — answer the current request

TODO: not sure about this. The struct passed to the entrypint can take a return value, but 'data' is wrong - no copy if we can avoid it. Top priority is to port the packet pool from dmesh - so module can send messages. I don't think they need to 'respond', just send (like they do for log).

**Returns:** `int` — `OK`, `ERR_BUSY` if already answered, `ERR_ARG` if the call was not a request.

The answer to a call with `flags` bit 0. By convention one CBOR item (a map keyed by field tags, like the
request), delivered to whoever posted the request. No answer means an empty one.

### Arguments

| Name | Type | Description |
|---|---|---|
| `data` | `const uint8_t *` | The answer (copied). |
| `len` | `size_t` | Length. |

## 5. `emit` — send an event to the application

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

## 6. `call_service` — call a service of the application by tag

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

## 7. `get_setting` — read a persistent setting of this module

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

## 8. `set_setting` — write a persistent setting of this module

**Returns:** `int` — `OK` or an error.

An empty value erases the setting.

### Arguments

| Name | Type | Description |
|---|---|---|
| `key` | `const uint8_t *` | Name. |
| `key_len` | `size_t` | Length. |
| `value` | `const uint8_t *` | Value. |
| `value_len` | `size_t` | Length. |

## 9. `gpio_config` — configure a pin

**Returns:** `int` — `OK`, `ERR_BUSY` if another module owns it, `ERR_UNSUPPORTED` if main reserved it.

Also sets the initial level of an output. The pin belongs to the module until the instance is destroyed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | GPIO number. |
| `mode` | `int` | `GPIO_*`. |
| `pull` | `int` | `GPIO_PULL_*`. |
| `level` | `int` | Initial output level. |

## 10. `gpio_read` — read a pin

**Returns:** `int` — level 0/1, or a negative error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | A pin configured by this module. |

## 11. `gpio_write` — set an output level

**Returns:** `int` — `OK` or an error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | A pin configured by this module. |
| `level` | `int` | 0 or 1. |

## 13. `irq_unregister` — stop reporting a pin

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The pin. |

## 12. `irq_register` — report the edges of a pin

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

## 14. `irq_enable` — mask or unmask a registered pin

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The pin. |
| `enabled` | `int` | 1 unmask, 0 mask. |

## 15. `spi_open` — open a full-duplex SPI device

**Returns:** `int` — a handle (>= 0) or a negative error.

The first open of a controller initialises it with the pins of the config; later opens must use the same
pins. A controller that main initialised is shared as is.

### Arguments

| Name | Type | Description |
|---|---|---|
| `config` | `const espmod_spi_config *` | See `spi_config`. |

## 16. `spi_transfer` — one chip-select cycle

**Returns:** `int` — `OK`, `ERR_BUSY` when `ready_pin` never became ready, `ERR_IO`.

`tx` and `rx` are both `len` bytes (`rx` may be NULL). Bounded; waits for `ready_pin` when configured.

### Arguments

| Name | Type | Description |
|---|---|---|
| `handle` | `int` | From `spi_open`. |
| `tx` | `const uint8_t *` | Bytes to send. |
| `rx` | `uint8_t *` | Bytes received, or NULL. |
| `len` | `size_t` | Length. |

## 21. `alarm_set` — call me later, once or periodically

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

## 22. `alarm_cancel` — cancel an alarm

**Returns:** `int` — `OK`, `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `alarm` | `int` | An id from `alarm_set`. |

## 23. `connected` — does main have a network path

**Returns:** `int` — 1 yes, 0 no.

Cheap. Use `ALARM_CONNECTED` to defer work until it is true.

## 17. `spi_close` — release an SPI device

**Returns:** `int` — `OK`, `ERR_ARG`.

Also done when the instance is destroyed.

### Arguments

| Name | Type | Description |
|---|---|---|
| `handle` | `int` | From `spi_open`. |

## 18. `i2c_transfer` — write then read on an I2C bus

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

## 19. `adc_read` — one-shot 12-bit reading

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

## 20. `rgbled_write` — set a WS2812-style LED

**Returns:** `int` — `OK` or an error.

### Arguments

| Name | Type | Description |
|---|---|---|
| `pin` | `int` | The data pin. |
| `red` | `uint8_t` | 0..255. |
| `green` | `uint8_t` | 0..255. |
| `blue` | `uint8_t` | 0..255. |

## 24. `wifi_start` — join a Wi-Fi network

**Returns:** `int` — `OK` (joining continues in the background), `ERR_UNSUPPORTED` without Wi-Fi, `ERR_BUSY` if already started.

Joins the named network and keeps rejoining until `wifi_stop`. The call returns at once (joining takes seconds); poll
`wifi_info`. The caller keeps the name and passphrase in its own settings, never in an image: an empty `ssid` means the flash
network (`DIRECT-xy-flash`, hosted by the flashing phone); an empty `psk` means that network's public passphrase. Nothing is on
the network until a module asks.

### Arguments

| Name | Type | Description |
|---|---|---|
| `ssid` | `const uint8_t *` | Network name, or empty. |
| `ssid_len` | `size_t` | Length (at most 32). |
| `psk` | `const uint8_t *` | Passphrase, or empty. |
| `psk_len` | `size_t` | Length (at most 63). |

## 25. `wifi_stop` — leave the network and switch the radio off

**Returns:** `int` — `OK`; the radio is released in the background.

## 26. `wifi_info` — state of the Wi-Fi

**Returns:** `int` — the length of the answer, or a negative error (`ERR_SIZE`: the buffer is too small).

One CBOR map: `{1: state, 2: ssid, 3: ip, 4: rssi}`: `state` is 0 off, 1 joining, 2 up; the other fields are present once
up (`ip` is the IPv4 address as text, `rssi` is signed dBm).

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

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
and starts and stops all of it. Each line becomes a request `(component, method)` to this module, `data` the line without
a newline. Whatever the handler `respond`s is written back to the port as text. The port is released by `serial_close` or
when the module ends.

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

## 32. `call` — send a request to another running module and take its answer

**Returns:** `int` — `OK`, `ERR_NOTFOUND` (no running module has that component), `ERR_BUSY` (it is already inside a request), `ERR_SIZE` (the answer did not fit).

The target's handler runs now, inside this call, on the same worker: one handler at a time is still true, they only nest.
So it is bounded like any handler, and the stack of the worker has to hold both. `component` is the target's slot tag; the
request and the answer are whatever bytes that module's `API.md` defines (this host moves them and reads neither).

### Arguments

| Name | Type | Description |
|---|---|---|
| `component` | `int` | The target module's component (slot tag). |
| `method` | `int` | Its method. |
| `data` | `const uint8_t *` | Request. |
| `len` | `size_t` | Length. |
| `response` | `uint8_t *` | Buffer for the answer. |
| `response_capacity` | `size_t` | Its size. |
| `response_len` | `size_t *` | Answer length. |

## 33. `modules_info` — every module slot

**Returns:** `int` — the length of the answer, or a negative error.

One CBOR array with a map per slot, keyed as the `slot` record of `modules/mod_console/API.md`: name, tag, installed, running,
version, last result, runs, duration of the last run, stack high-water mark, result of the last image check.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 34. `module_start` — start the module installed in a slot

**Returns:** `int` — `OK` (the module starts in the background), `ERR_NOTFOUND` for an unknown name, or the loader's error.

`request` is what the module receives with `HOST_START`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Slot name. |
| `name_len` | `size_t` | Length. |
| `request` | `const uint8_t *` | Start request, may be empty. |
| `request_len` | `size_t` | Length. |

## 35. `module_stop` — ask a module to end

**Returns:** `int` — `OK` (it ends in the background; poll `modules_info`), `ERR_NOTFOUND`.

### Arguments

| Name | Type | Description |
|---|---|---|
| `name` | `const uint8_t *` | Slot name. |
| `name_len` | `size_t` | Length. |

## 36. `events_info` — the last events modules emitted

**Returns:** `int` — the length of the answer.

One CBOR array, oldest first, of maps keyed as the `event` record of `modules/mod_console/API.md`: module, event id, and the
value as text (a text value as it is, any other as hex). The host keeps the last 16.

### Arguments

| Name | Type | Description |
|---|---|---|
| `out` | `uint8_t *` | Buffer. |
| `capacity` | `size_t` | Its size. |

## 37. `reset` — restart the device

**Returns:** `int` — `OK`; the restart follows in 300 ms.

# Helpers

Not host calls: header-only code in `include/espmod_cbor.h` (C) and `espmod-abi`'s `cbor` module (Rust), for
modules and hosts alike.

* Encoder: `em_cbor_uint`, `em_cbor_int`, `em_cbor_bytes`, `em_cbor_text`, `em_cbor_map`, `em_cbor_array`,
  `em_cbor_bool` write to a bounded buffer and report overflow once, at the end.
* Decoder: `em_cbor_next` reads one item (type, integer, or a pointer and length for bytes and text) and
  `em_cbor_skip` steps over a whole item; `em_cbor_field` finds the value of an unsigned-integer key in a map.
* Only definite lengths, integers up to 64 bits, byte and text strings, arrays, maps and booleans; floats and
  tags are skipped but not decoded.
