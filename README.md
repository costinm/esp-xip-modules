# esp-xip-modules

https://github.com/costinm/esp-xip-modules

Small, signed programs ("modules") that ESP32 executes in place from flash and upgrades independently.

This is not an OS - but a library that can be used in other apps (or OSes). Many OSes provide
loadable ELF apps - run from RAM and specific to the OS. This is XIP, since RAM is too valuable,
and (mostly) OS-independent. A small benefit is that modules can also be tested on the host.

A module sees the hardware only through one table of functions (GPIO, interrupts, SPI, I2C, ADC, LED, alarms,
settings, time, events), so the same module source runs on an ESP32, ESP32-S3 and ESP32-C6. It never waits, sleeps
or loops: the host calls it when something happens and sleeps when nothing does, which is what a battery-powered
device with wake windows needs. A module can also be linked into an application as a library, but modules are best as separate upgrade units. C and Rust modules are both supported.

This repo provides a C 'main' - as a working example and for testing - along with a C runtime implementing the modules ABI.

It also has a Rust 'main' and runtime - with some extended functionality, as a super-set of the C runtime which is intended
to be minimal.

Both runtimes support C and Rust modules - I suspect Zig and other native languages would also work with some work.

## Goals

- Efficient loadable modules that can be developed and upgraded independent of the main runtime and application.
- Secure and fast upgrade of modules
- Encourage efficient code: async/non-blocking, zero copy, separate protocol/encoding from handlers, pay for what is used, etc.

Ideally most of the application logic will be developed in modules, with the runtime and core components ugrading
infrequently and smaller, and the set of modules that a user needs upgraded more frequently and based on what is used.

Non goals:
- Not a OS or 'framework' - just a library providing an API/ABI that can be embedded and used in other projects including OSes


## What it is good for

- A device whose behaviour you want to change without rebuilding Main: a radio driver, a sensor, a button policy.
- Keeping hardware-specific code out of the application: `main` stays small and stable, modules change.
- Trying a driver on a bench board and then shipping the same source linked into a product.
- An optional owner-controlled installer can verify signed images before writing them.

## Using it

```sh
. ./env.sh                         # ESP-IDF v6.0.2 from this checkout or EM_ESP_ROOT
scripts/build.sh esp32c6  # main + the modules in target/out/esp32c6/
```

Install `main` with the esp-wifi-loader flow or esptool. The sample Main starts no network or upload listener at boot; a module can request its optional STA Wi-Fi ABI.
Install a module through the loader or write its slot with esptool, then use the serial console:

```sh
python3 tools/console.py send "start test"   # the device test, over the serial console
python3 tools/console.py send "events"
python3 tools/console.py send "test.echo value=hi"    # a method of a module, like the ssh-mesh CLI
```

`docs/getting-started.md` goes through it step by step. To write your own module, copy `modules/mod_hello` (C) or
`modules/mod_hw` (Rust) and read `docs/writing-modules.md`.

## Code layout

Extracted and simplified from the module system of [costinm/dmesh](https://github.com/costinm/dmesh). It is
built like its sibling [esp-wifi-loader](https://github.com/costinm/esp-wifi-loader), which installs `main`.

| Path | What |
|---|---|
| `abi/API.md` | the module ABI, written like an ssh-mesh `API.md`: the one document everything else is generated from |
| `include/espmod.h` | the C header generated from it; `include/espmod_cbor.h` a small CBOR encoder/decoder |
| `crates/espmod-abi/` | Rust: the generated table, a safe wrapper, `Module` + `export_module!`, CBOR |
| `crates/espmod-host/` | Rust: start the host and the loader over a slot table, start/stop/call modules (bytes only, no CBOR); `example/` is the Rust twin of `main` |
| `components/espmod_host/` | ESP-IDF component: the hardware and the instances: the host table, the worker task, alarms, interrupts, SPI, I2C, ADC, LED, settings, the serial port |
| `components/espmod_pool/` | ESP-IDF-free C: the packet pool and the table of packets a module holds ([docs/packets.md](docs/packets.md)) |
| `components/espmod_loader/` | ESP-IDF component: only the MMU and the slots: maps a module from flash, starts and stops it, replaces the image of a slot |
| `main/` | `main`: an example and test driver with a slot table and generic host hooks. It does not start a network or uploader. |
| `modules/` | `mod_hello` (the smallest), `mod_test` (the device test), `mod_console` (the only text: the serial console, linked into the application), `mod_lora` and `mod_hw` (Rust) |
| `tools/` | `mkmod.py` (build a module), `console.py` (the serial console: monitor, send a command), `gen_abi.py`, `gen_catalog.py`. Signing, uploading, flashing and resetting are **esp-wifi-loader's** scripts (`sign.py`, `upload.py`, `provision.py`, `reset.py`, `Dockerfile.sign`): no copies here; `. ./env.sh` puts them on the PATH (`ESP_WIFI_LOADER_DIR`, default `../esp-wifi-loader`) |
| `tests/` | host tests (no board) and the bench tests (`device_test.py`, `lora_test.py`) |
| `scripts/` | `build.sh`, `test-host.sh`, `publish-release.sh`: CI calls only these |
| `docs/` | concepts, putting `main` on a board, how to write a module, signing and releasing, protocol |

## Status

- Builds for ESP32, ESP32-S3 and ESP32-C6 (ESP-IDF v6.0.2); modules build for all three with the checks a module
  must pass (no relocations, no libc, no writable data).
- Host tests cover the ABI generator and catalog, modules against a simulated host, the console, packet pool,
  and image checks; the Rust crates have their own `cargo test`.
- **Run on boards:** ESP32-C6 (bench board `e8`) with the first, task-per-module model; **ESP32 (two LoRa boards)**
  with the event-driven model: `main` through esp-wifi-loader, `mod_test` from flash (39 checks, alarms,
  requests, hot replace and refusals), the text console, and `mod_lora` / `mod_hw` (two boards exchanging packets in both
  directions, in the middle of a reception and with CAD windows: `tests/lora_test.py`).
- **Not yet run on a board:** ESP32-S3 and ESP32-C6 with the event-driven model, SX126x, the wired GPIO / interrupt checks of `mod_test`, wake windows (the hook exists, no
  sleepy mode yet).
- Not here yet: a Rust `main`; a packet pool for zero-copy I/O (`espmod_call.data` is valid during the call only,
  which leaves room); network messaging to modules.

## High level implementation details.

## Module runtime environment

For ESP32 with Xtensa - Rust doesn't support PIC, so each module must have a specific
'slot ID', based on their CBOR tag number, and based on that it gets a specific 64k slot in flash and fixed base address in XIP. Large modules can take multiple slots. With RiscV - PIC works so location is flexible, but for now keeping the same model.

After main partition, the remaining space is divided into 64k blocks which can be mapped by 
the MMU. Current CPUs can cache about 32k total from all the flash.

## High level implementation details.

## Module runtime environment

For ESP32 with Xtensa - Rust doesn't support PIC, so each module must have a specific
'slot ID', based on their CBOR tag number, and based on that it gets a specific 64k slot in flash and fixed base address in XIP. Large modules can take multiple slots. With RiscV - PIC works so location is flexible, but for now keeping the same model.

After main partition, the remaining space is divided into 64k blocks which can be mapped by 
the MMU. Current CPUs can cache about 32k total from all the flash.

The modules are expected to be non-blocking and use interrupts and callbacks, as well as send
messages to other modules or devices.


## License

Apache-2.0.
