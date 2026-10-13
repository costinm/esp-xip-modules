# `console` API (20)

The text interface of a device, and the only place text is made or read. It takes the serial port from the host
(`serial_open`): each line typed arrives as a request (method 3, the line as bytes) and the text it answers is printed.
A line is one of the commands below or `component.method key=value ...` (the spelling of the ssh-mesh `mesh` CLI), which
becomes the tagged-CBOR request of the module `component` (names and tags from the `API.md` files of the modules,
`tools/gen_catalog.py`), and is sent as a packet with an id. Nothing waits for an answer: whatever packets modules send to the
console, one or many, answers or not, are printed as they arrive. Before sending, the console asks the host for the module's
**manifest** (`module_manifest`: the methods the installed image says it has) and refuses a method that is not in it. `parse` (1) and `render`
(2) below are the two halves of that, for a caller that has its own transport.

Commands: `status` (the slots), `start NAME [TEXT]`, `stop NAME`, `events` (the last events modules emitted), `reset`,
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

# Types

## `slot` — one module slot (`status`; the host's `modules_info`)

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

## `event` — one event of a module (`events`; the host's `events_info`)

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `module` | `string` | |
| 2 | `id` | `uint` | The `event_id` of `emit`. |
| 3 | `text` | `string` | The text, or the bytes as hex. |
