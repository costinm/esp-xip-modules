# xip-abi

The module ABI, declared in Rust. The C header [include/xip.h](../../include/xip.h) is generated from it with
`scripts/gen.sh`. Modules for the C, Rust and host runtimes use `Hw` for hardware and services, and `task::Task` to write
handlers as `async` functions.

API reference: `cargo doc --manifest-path crates/xip-abi/Cargo.toml`. Command schemas: [api](../../api).
Examples: [mod_rtest](../../modules/mod_rtest/src/lib.rs). The guide to writing a module is kept with the project notes.
