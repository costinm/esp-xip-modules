# `lora` API (43)

The LoRa/FSK radio: SX127x and SX126x, receive, transmit, channel activity detection (CAD). Component 43 is the slot
tag (DMesh uses the same number). The module is event driven: a request starts work and returns; the radio's
interrupt line and the module's own alarms carry it on. Results of work that takes time are **events** (below), the
answer of a request only says whether it was accepted (`{1: 0}`; `-2` bad request, `-5` busy: one request waits behind
the work in progress).

Every method takes the same request: any of the settings 1..30 (they replace the module's value for the rest of the
session; without any the module uses the TTGO T-Beam/LoRa32 SX127x wiring and US MediumFast) and the data 31.
`start lora rx freq=915000000 sf=9` (the older text form, see the README) does the same at start; a bare `start lora` probes, `start lora idle` only brings the module up (what an application does before its first method call).

## 1. `probe` — read the chip

**Request:** [Radio](#radio)
**Response:** [Result](#result)

Brings the board up (power rail, reset, SPI) and reads the chip version. Logs `mod_lora probe ok` or `probe failed`;
event 31 carries the chip, event 6 a failure.

## 2. `probe127` — read an SX127x version register

**Request:** [Radio](#radio)
**Response:** [Result](#result)

## 3. `probe126` — read an SX126x status

**Request:** [Radio](#radio)
**Response:** [Result](#result)

## 4. `rx` — receive until `stop`

**Request:** [Radio](#radio)
**Response:** [Result](#result)

Configures the radio and listens; every packet is delivered as event 10 (CBOR `[rssi, snr, bytes]`). With `cad` 1
the radio listens with CAD windows and opens a receive window when one detects a preamble. Event 1 says reception
started.

## 5. `tx` — transmit one packet

**Request:** [Radio](#radio)
**Response:** [Result](#result)

During `rx` the packet is sent and reception resumes afterwards. Event 3 carries the packet when it was sent; event 6
a failure (an SX127x transmission that never ends reports -4, an SX126x one -1000 minus its IRQ bits).

## 6. `stop` — release the radio

**Request:** [Radio](#radio)
**Response:** [Result](#result)

Cancels what is running, powers the rail down and releases the SPI device. Event 2. A later request brings the board
up again.

## 7. `reconfigure` — apply the settings to the running mode

**Request:** [Radio](#radio)
**Response:** [Result](#result)

Event 4.

## 8. `fsk` — FSK (100 kbit/s GFSK)

**Request:** [Radio](#radio)
**Response:** [Result](#result)

With data one packet is sent in FSK and the previous mode resumes; without data the radio receives FSK until `stop`
(events 22 packet, 23 receive error). `tx` during an FSK session sends FSK too.

## 9. `stats` — counters

**Request:** [Radio](#radio)
**Response:** [Result](#result)

Event 5: receive and transmit packet counters (two little-endian u32).

# Events

| Id | Meaning | Value |
|---:|---|---|
| 1 / 2 | receive started / stopped | |
| 3 | packet sent | the packet |
| 4 | reconfigured | |
| 5 | stats | rx, tx counters |
| 6 | setup, transmit or probe error | i32 |
| 7 | CAD: quiet (every 20th window) | i32 |
| 8 | CAD detected activity but no packet came | i32 |
| 9 | CAD error | i32 |
| 10 | **packet received** | CBOR `[rssi, snr, bytes]` |
| 11, 32+ | SX126x CAD window that never finished | status |
| 20 | FSK transmission starts | length |
| 22 / 23 | FSK packet / receive error | |
| 31 | chip found by `probe` | i32 |

# Types

## `Radio`

Every method takes the same request: any of the settings 1..30 (they replace the module's value for the rest of the session) and the data 31.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `chip` | `uint` | 1 SX127x (default), 2 SX126x. |
| 2 | `freq` | `uint` | Hz. Default 913125000. |
| 3 | `bw` | `uint` | Bandwidth, Hz. Default 250000. |
| 4 | `sf` | `uint` | Spreading factor. Default 10. |
| 5 | `spi` | `uint` | SPI controller, 2 or 3. Default 3. (Before the esp-xip-modules ABI the field was 1/2 for SPI2/SPI3.) |
| 6 | `sync` | `uint` | LoRa sync word. Default 0x2b. |
| 7 | `power` | `uint` | dBm. Default 17. |
| 8 | `rst` | `int` | Reset pin. |
| 9 | `cs` | `int` | |
| 10 | `irq` | `int` | The radio's interrupt line (DIO0 / DIO1). |
| 11 | `busy` | `int` | SX126x BUSY; the host waits for it as the SPI ready pin. |
| 12 | `sck` | `int` | |
| 13 | `miso` | `int` | |
| 14 | `mosi` | `int` | |
| 15 | `pwr` | `int` | Board power rail pin, -1 none. |
| 16 | `pwrlvl` | `uint` | Level that powers the rail. |
| 17 | `dio2` | `uint` | SX126x: DIO2 as RF switch. |
| 18 | `tcxo` | `uint` | SX126x: TCXO voltage in mV (DIO3). |
| 19 | `pa_duty` | `uint` | SX126x PA settings. |
| 20 | `pa_hp` | `uint` | |
| 21 | `pa_dev` | `uint` | |
| 22 | `pa_lut` | `uint` | |
| 23 | `sx_sync` | `uint` | SX126x sync word. Default 0x24b4. |
| 24 | `rx_timeout` | `uint` | SX126x receive timeout. |
| 25 | `cr` | `uint` | Coding rate 4/5..4/8, as 5..8. |
| 26 | `preamble` | `uint` | Symbols. Default 16. |
| 27 | `crc` | `uint` | 0/1. |
| 28 | `cad` | `uint` | 1: listen with CAD windows instead of continuous receive. |
| 29 | `cad_interval` | `uint` | Longest wait for a CAD window, ms. |
| 30 | `cad_rx_ms` | `uint` | Receive window after a detection, ms. |
| 31 | `data` | `bytes` | The packet (`tx`, `fsk`). |
| 32 | `cad_period` | `uint` | Pause between two CAD windows, ms. Default 25 (the chip and the CPU idle; keep it under the preamble time). |

## `Result`

The answer only says whether the request was accepted.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `result` | `int` | 0 accepted. |
