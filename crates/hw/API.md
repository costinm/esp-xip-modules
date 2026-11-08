# `hw` API (45)

Hardware policy that does not belong in the application: the battery level behind a voltage divider, an ADC probe
and a button. Component 45 is the slot tag (DMesh uses the same number). Requests are tagged CBOR maps, answers
too; measurements are also emitted as events (value type CBOR array).

Settings the module reads when a request omits a field: `battery.pin`, `battery.ref_mv`, `battery.divider`
(decimal, `2.2`) or `battery.divider_x100`, `battery.min_mv`, `battery.max_mv`, `battery.ctrl`, `battery.ctl_lvl`,
`button.gpio`. The module uses the `hw` component (ADC) and the settings.

An error is answered as `{9: code}` (a negative `ERR_*` of the ABI or `-2` bad request, `-4` empty range, `-5`
control pin).

## 1. `battery` — measure the battery

Reads `pin` (a 12-bit one-shot reading), scales it by the divider and reports millivolts and a percentage between
`min_mv` and `max_mv` (255 = unknown). With a control pin the divider is switched on first and the measurement
follows 10 ms later (an alarm): the answer is then empty and the result arrives only as event 111.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `pin` | `int` | ADC pin; default setting `battery.pin`, else 35 (ESP32) / 0 (ESP32-C6). |
| 2 | `ref_mv` | `uint` | Reference in millivolts. Default 3300. |
| 3 | `divider_x100` | `uint` | Divider ratio times 100 (220 = 2.2). |
| 4 | `min_mv` | `uint` | Empty battery. Default 3300. |
| 5 | `max_mv` | `uint` | Full battery. Default 4200. |
| 6 | `ctrl_pin` | `int` | Pin that enables the divider, -1 = none. |
| 7 | `ctrl_level` | `uint` | Level that enables it. Default 1. |
| 8 | `enabled` | `uint` | 0 does nothing (a disabled feature). |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `pin` | `uint` | |
| 2 | `raw` | `uint` | ADC counts. |
| 3 | `mv` | `uint` | At the pin. |
| 4 | `battery_mv` | `uint` | After the divider. |
| 5 | `level` | `uint` | Percent, 255 unknown. |
| 6 | `ref_mv` | `uint` | |
| 7 | `unit` | `uint` | ADC unit. |
| 8 | `channel` | `uint` | ADC channel. |

## 2. `probe` — sample pins on a timer

The first round is taken at once and answered by events (110, one per pin: the same array as 111 without the
battery values); further rounds follow every `interval_ms` until `samples` rounds were taken. `samples` 0 means
until `stop` or the next `probe`.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `samples` | `uint` | Rounds. Default 1. |
| 2 | `interval_ms` | `uint` | Between rounds, 1..60000. Default 1000. |
| 3 | `ref_mv` | `uint` | Default 3300. |
| 4 | `pins` | `array` | Up to four ADC pins. Default 34, 35, 36, 39. |

## 3. `button` — report presses

Configures the pin as an input with a pull-up and reports presses as events: 101 short, 102 long (held 2.5 s or
more), 103 double (two releases within 500 ms); each is `[pin, held_ms, 0]`. A new `button` request replaces the
old one; `enabled` 0 stops reporting.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `pin` | `int` | Default setting `button.gpio`, else 0. |
| 2 | `enabled` | `uint` | 0 stops. |
