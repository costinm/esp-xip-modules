# mod_lora

The LoRa/FSK radio module: SX127x and SX126x drivers, packet reception and transmission, CAD receive, the
board bring-up (power rail, reset, SPI with the SX126x BUSY line). **All LoRa code is here**; the host
(`esp-xip-modules`) and the applications that run it (DMesh) know nothing about it.

It is an [esp-xip-modules](https://github.com/costinm/esp-xip-modules) module: one entry function that talks to the board only
through the esp-xip-modules host table (GPIO, interrupts, SPI, time, events). The same code runs

| Form | How |
|---|---|
| flash module `mod_lora.dmod` | `scripts/build.sh esp32c6` (needs the ESP Rust toolchain: `XIP_RUST_MODULES=1`), or DMesh's `scripts/build-modules.sh`, signed and installed through the loader (below) |
| linked into an application | an instance whose handler is `esp_mod_lora::xip`'s exported handler (`xip_instance_create`) |
| host tests | `cargo test` in this directory: the drivers run against a scripted fake board |

## Layout

| File | Content |
|---|---|
| `hal.rs` | the `Hal` trait: what the drivers need (SPI, pins, delay, interrupt wait, requests, events), shaped like embedded-hal |
| `config.rs` | radio and board settings, defaults (TTGO SX127x wiring, US MediumFast), field ids |
| `command.rs` | the request format (below) and its parser |
| `sx127x.rs`, `sx126x.rs`, `fsk.rs` | the chip drivers (LoRa mode; FSK of both chips) |
| `run.rs`, `entry.rs` | the RX loops and the entry (`rx`, `tx`, `probe`, ...) |
| `xip.rs` | `Hal` over the esp-xip-modules host table, and the `xip_entry` adapter: the only file that sees the C ABI |
| `frames.rs` | Meshtastic-style framing helpers |

## Requests

A request starts the module (`start`) or is sent to the running one (`post`). Two forms, same content:

```text
rx                                   # text: operation, then name=value settings, data=HEX or text=WORD
tx freq=915000000 sf=9 text=hello
probe chip=2 busy=13 pwr=21 rst=12 cs=8 spi=2
{0: "tx", 2: 915000000, 4: 9, 31: h'68656c6c6f'}      # CBOR map: 0 operation, 1..30 settings, 31 data
```

Operations: `probe`, `probe127`, `probe126`, `rx` (stays running until `stop`), `tx`, `stop`, `reconfigure`, `fsk`,
`stats`. Settings are not persisted by the host: every start carries what it needs, and a setting in a later
request (`reconfigure freq=868100000`) replaces the running value. The names and ids are in `config.rs`
(`chip freq bw sf spi sync power rst cs irq busy sck miso mosi pwr pwrlvl ...`). Without settings the module uses the
SX127x wiring of the TTGO T-Beam/LoRa32 boards (`sck 5, miso 19, mosi 27, cs 18, rst 14, irq 26`, SPI3).

## Events

| Id | Meaning | Value |
|---:|---|---|
| 1 / 2 | RX started / stopped | - |
| 3 | packet sent | the packet |
| 4 | reconfigured | - |
| 5 | stats | rx, tx counters (two u32, little endian) |
| 6 | TX or setup error | i32 |
| 10 | **packet received** | CBOR `[rssi, snr, bytes]` |
| 30, 31 | request/diagnostics | i32 |

DMesh's `fw/main/src/modules.rs` routes event 10 of the `lora` module to the transport.

## Installing

The module is signed (esp-xip-modules `sign.py`) and uploaded to a running `main`; see the esp-xip-modules
`docs/getting-started.md`. On a DMesh Main the slot is `lora` (tag 43); on an esp-xip-modules `main` it is the `lora` slot of `main/main.c`. A bench run, with results, is in
[notes/ai/2026-10-11-mod-lora-on-esp-modules.md](../../notes/ai/2026-10-11-mod-lora-on-esp-modules.md).

## Hardware notes

- SX126x: `busy` is passed to the host as the SPI *ready pin*; the host waits for it before and after each transfer.
- The chip reset and the optional board power rail are driven by the module (`pwr`, `pwrlvl`, `rst`).
- `stop` powers the rail down and releases the pins; the host releases whatever the module leaves.
- FSK uses a fixed 100 kbit/s GFSK profile on a common channel; framing beyond fixed packets is a follow-up.
