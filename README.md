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

This repo provides a C main - as a working example and for testing, with a set of C and Rust modules. The original project (costinm/dmesh) has a Rust main using this repo as a library along with the modules.

Extracted and simplified from the module system of [costinm/dmesh](https://github.com/costinm/dmesh). It is
built like its sibling [esp-wifi-loader](https://github.com/costinm/esp-wifi-loader), which installs `main`.

| Path | What |
|---|---|
| `abi/API.md` | the module ABI, written like an ssh-mesh `API.md`: the one document everything else is generated from |
| `include/espmod.h` | the C header generated from it; `include/espmod_cbor.h` a small CBOR encoder/decoder |
| `crates/espmod-abi/` | Rust: the generated table, a safe wrapper, `Module` + `export_module!`, CBOR |
| `crates/espmod-host/` | Rust: start the host and the loader over a slot table, start/stop/call modules (bytes only, no CBOR); `example/` is the Rust twin of `main` |
| `components/espmod_host/` | ESP-IDF component: the hardware and the instances: the host table, the worker task, alarms, interrupts, SPI, I2C, ADC, LED, settings, the serial port |
| `components/espmod_loader/` | ESP-IDF component: only the MMU and the slots: maps a module from flash, starts and stops it, replaces the image of a slot |
| `components/espmod_install/` | ESP-IDF component: writes a package that esp-wifi-loader verified into a slot |
| `main/` | `main`: an example and test driver: a slot table and the wiring; modules arrive through esp-wifi-loader and the console module. No network control port. Start a product here |
| `modules/` | `mod_hello` (the smallest), `mod_test` (the device test), `mod_console` (the only text: the serial console, linked into the application), `mod_lora` and `mod_hw` (Rust) |
| `tools/` | `mkmod.py` (build a module), `console.py` (the serial console: monitor, send a command), `gen_abi.py`, `gen_catalog.py`. Signing, uploading, flashing and resetting are **esp-wifi-loader's** scripts (`sign.py`, `upload.py`, `provision.py`, `reset.py`, `Dockerfile.sign`): no copies here; `. ./env.sh` puts them on the PATH (`ESP_WIFI_LOADER_DIR`, default `../esp-wifi-loader`) |
| `tests/` | host tests (no board) and the bench tests (`device_test.py`, `lora_test.py`) |
| `scripts/` | `build.sh`, `test-host.sh`, `publish-release.sh`: CI calls only these |
| `docs/` | concepts, putting `main` on a board, how to write a module, signing and releasing, protocol |

## What it is good for

- A device whose behaviour you want to change in the field in seconds, over Wi-Fi, without a full firmware
  update: a radio driver, a sensor, a button policy.
- Keeping hardware-specific code out of the application: `main` stays small and stable, modules change.
- Trying a driver on a bench board and then shipping the same source linked into a product.
- An untrusted network: only images signed with your key are ever installed (ECDSA P-256).

## Using it

```sh
. ./env.sh                         # ESP-IDF v6.0.2 (reuses a DMesh or esp-wifi-loader install)
sign.py keygen target/bench.pem            # a bench key; the real key is yours (docs/releasing.md)
sign.py pubkey target/bench.pem > target/bench.pub
scripts/build.sh esp32c6  # main + the modules in target/out/esp32c6/
```

Install `main` with the esp-wifi-loader flow from an Android phone (`docs/flashing.md`); type `wifi start` and `loader start NAME` on its serial console to open the
network and the uploader; `wifi` prints its address. Then, with that address:

```sh
sign.py sign --key target/bench.pem --cpu esp32c6 --version 1 --target module --name test \
    --image target/out/esp32c6/modules/mod_test.dmod --out test.wlpkg
upload.py send test.wlpkg --host ADDRESS       # verified, installed, nothing rebooted
python3 tools/console.py send "start test"   # the device test, over the serial console
python3 tools/console.py send "events"
python3 tools/console.py send "test.echo value=hi"    # a method of a module, like the ssh-mesh CLI
```

`docs/getting-started.md` goes through it step by step. To write your own module, copy `modules/mod_hello` (C) or
`modules/mod_hw` (Rust) and read `docs/writing-modules.md`.

## Status

- Builds for ESP32, ESP32-S3 and ESP32-C6 (ESP-IDF v6.0.2); modules build for all three with the checks a module
  must pass (no relocations, no libc, no writable data).
- Host tests cover the ABI generator and catalog, the modules against a simulated host with virtual time (alarms,
  interrupts, requests), the console, the control surface, the image and package checks, and the whole device side of
  the upload protocol against a RAM flash; the Rust crates have their own `cargo test`.
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
