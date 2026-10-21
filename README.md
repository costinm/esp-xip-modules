# esp-xip-modules

https://github.com/costinm/esp-xip-modules

Small programs ("modules") that ESP32 chips execute in place from flash and upgrade independently.

This is not an OS: it is a library that applications (or OSes) embed. Many OSes load ELF apps into RAM and tie them to
that OS. Modules run in place from flash, because RAM is too valuable, and are mostly OS independent. They can also be
tested on the host.

The runtime gives modules hardware and services through function tables. Modules are event driven and non-blocking,
written as plain handlers or as `async` functions, and can also be linked into an application as libraries. They run on
ESP32, ESP32-S3 and ESP32-C6, and on Linux with the host runtime.

## Goals

- Modules that are developed and upgraded independently of the runtime and the application.
- Secure, fast upgrade of modules.
- Efficient code: async and non-blocking, zero copy, protocol and encoding separate from handlers, pay for what is used.

Most application logic should live in modules. The runtime and core components change rarely and stay small.

Non-goal: an OS or framework. This is an API/ABI that other projects embed.

## Using it

```sh
. ./env.sh
scripts/build.sh esp32c6                      # runtime image and all modules, under target/out/esp32c6
scripts/build-host-modules.sh                 # the same modules for Linux
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host
```

`xip-host` runs the console and an echo module on loopback TCP and UDP; see
[the host runtime](crates/xip-runtime-host/README.md). `scripts/test-host.sh all` runs the tests that need no board.

The module interface is [include/xip.h](include/xip.h), generated from the Rust declarations in
[crates/xip-abi](crates/xip-abi/README.md). [modules/mod_flash](modules/mod_flash/README.md) installs images over TCP (development).

## License

Apache-2.0.
