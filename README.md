# esp-xip-modules

https://github.com/costinm/esp-xip-modules

Small programs ("modules") that ESP32 chips execute in place from flash and upgrade independently.

This is not an OS: it is a library that applications (or OSes) embed. Many OSes load ELF apps into RAM and tie them to
that OS. 

Modules run in place from flash, because RAM is too valuable, and are mostly OS independent. They can also be
run on linux/android - compiled as WASM, making development/testing easier. On ESP32 modules are updated and
developed independntly for the runtime - which is ~900k and provides WiFi and hardware ABI. That is faster and
simpler, both for development and for upgrades.  

The low RAM use and modules have some costs, given the limitations of ESP32:
- modules must be on 64k-aligned flash blocks (multiple contiguous blocks are ok) - MMU maps 64k blocks
- app must maintain a list of all modules with 64k 'slot' numbers - no PIC for Xensa and avoids runtime linker overhead.
- event driven, non-blocking Rust no-std, alloc, async code. Also C and probably other languages, similar style.
- the module and deps can't have statics  (.bss/.data) - require RAM mapped even if module is not active.

The last is the most difficult - problem with '.bss' is that it requires a specific area of memory to be allocated
for the lifecycle of the module - there is no 'process' that can be stopped are restarted, and no easy way 
to reuse the memory give the MMU limitations.

In exchange of those limitations, you can develop, deploy, upgrade small modules independently, test them
on host - and use far less RAM than a traditional app.

## Goals

- Small modules that are developed and upgraded independently
- Safe, fast upgrade of modules.
- Efficient code: async and non-blocking, zero copy, protocol and encoding separate from handlers, pay for what is used.

Application logic should live in modules. The runtime and core components change rarely and stay small.

Non-goal: an OS or framework. This is an API/ABI that other projects embed.

## Networking

The runtime includes a 'packet pool'. Wifi, BLE, serial as well as inter-module communication borrow and return
packets, with Rust-like ownership.

TCP/UDP are supported - along with a minimal version of Quic that also works over serial, BLE, LoRa, FSK and other
non-IP based transport, as well as over ESP-NOW. Reasons for using Quic:

- can switch transports as needed
- more flexible and tolerant with packet loss than IP

Using IP everywhere doesn't mean all local links must use TCP - devices an have a virtual IPv6 address but
tunnel of non-IP transports, including multi-path and streams that are more tolerant to out-of-order packets.

Most of the module interface is 'RPC/gRPC/Json-rpc'-like - using CBOR for encoding and the quic-lite as
transport. 

Of course, since quic-lite is a module - it can be replaced and is not required by the runtime, and the
provided modules do not depend on it - just on an abstract 'stream' interface.

CBOR is part of the runtime - we need something that is compact and standard, can be swapped with 
another encoding if needed.

## Power management

The most important module is 'mod_nan', which implements a subset of WifiAware/NAN, sufficient
to communicate with Android and other ESP32 devices - creating a cluster aligned on the same beacons.
It can't create a NAN cluster - no beacon support - however it can use ESP-NOW action frames 
carying the same payload to create an equivalent cluster in the absence of an Android master.

This allows ESP32 devices to sleep, waking up ~500ms or 4s for short intervals.

Some devices may stay active for longer, in they are powered.

Runtime provides the APIs to control sleep and schedule - the module is not required.

## Runtimes

ESP32 run a ~800k runtime - goal is to keep it under 1M, providing the loader, glue and
a set of core 'components'.

Each module can provide one or more components. Each component is a set of method with
a common RPC-like interface.

The same components and modules run on the host - currently compiled to WASM, in the past
they used .so files but WASM is closer and may also be supported on device.

## Using it

```sh
. ./env.sh
scripts/build.sh esp32c6                      # Main, recovery and all modules under target/out/esp32c6
scripts/build-host-modules.sh                 # the same modules for Linux
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host
```

`xip-host` runs the console and an echo module on loopback TCP and UDP; see
[the host runtime](crates/xip-runtime-host/README.md). `scripts/test-host.sh generated` and `scripts/test-host.sh ci-runtime` run the maintained Rust host checks. `scripts/test-host.sh c` and `scripts/test-host.sh python` run the C and Python regressions. See [device tests](tests/e2e/README.md) for discovered C Main scenarios.

`main.bin` includes BLE by default; `loader.bin` is the recovery runtime without
BLE. `XIP_BLE_PROFILE=off` builds Main without BLE too. `main-rs/build.sh CHIP`
builds the Rust Main wrapper with the same default BLE profile. Radio startup
remains controlled by the installed modules and saved settings.

C Main is the execution target; `fw/` is retained to recover missing features.
CI builds C Main and modules for ESP32, ESP32-S3 and ESP32-C6, and tests
`dmesh-cli`. Use `scripts/test-host.sh cli` for CLI checks. See
[the workflow](.github/workflows/build.yml) for the build matrix.

The module interface is [include/xip.h](include/xip.h), generated from the Rust declarations in
[crates/xip-abi](crates/xip-abi/README.md). [modules/mod_flash](modules/mod_flash/README.md) installs images over TCP (development).

## License

Apache-2.0.
