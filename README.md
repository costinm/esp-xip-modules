# esp-xip-modules

https://github.com/costinm/esp-xip-modules

Small, signed programs ("modules") that ESP32 executes in place from flash and upgrades independently.

This is not an OS - but a library that can be used in other apps (or OSes). Many OSes provide
loadable ELF apps - run from RAM and specific to the OS. This is XIP, since RAM is too valuable,
and (mostly) OS-independent. A small benefit is that modules can also be tested on the host.

The runtime exposes hardware and services through function tables. Modules are
designed for event-driven, non-blocking execution and can also be linked into an
application as libraries. C and Rust modules are supported on ESP32, ESP32-S3 and
ESP32-C6.

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

Build the runtime and example modules:

```sh
. ./env.sh
scripts/build.sh esp32c6
```

Start with [mod_hello](modules/mod_hello) for C or [mod_hw](modules/mod_hw) for Rust.
[build-mod.sh](scripts/build-mod.sh) builds a module from its directory.
The public module interface is `include/xip.h` (generated from the Rust declarations in `crates/xip-abi`, documented there: `cargo doc -p xip-abi`); the CBOR, JSON and text view of the APIs is in [api/](api/).

The runtime starts installed modules that request auto-start. The optional console
provides device inspection and module control; the optional installer comes from
[esp-wifi-loader](https://github.com/costinm/esp-wifi-loader).

## License

Apache-2.0.
