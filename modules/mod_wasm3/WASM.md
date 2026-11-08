# Compiling the modules to WebAssembly

Only `modules/` is built, with plain `cargo`. The toolchain comes from the flake (`wasm-dev` package): Rust nightly
(the modules need `allocator_api`) with the `wasm32-unknown-unknown`, `wasm32v1-none` and `wasm32-wasip1` targets and
rust-src, plus binaryen (`wasm-opt`), wabt, `wasm-tools` and `wasmtime`. `rust-lld` ships with rustc.

## Install the tools

```sh
export WASM_DEV=${WASM_DEV:-$HOME/opt/wasm-dev}
nix --extra-experimental-features 'nix-command flakes' build .#wasm-dev --out-link $WASM_DEV
export PATH=$WASM_DEV/bin:$PATH
```

## Build a module

```sh
cd modules/mod_rtest
cargo rustc --release --lib --target wasm32-unknown-unknown --crate-type cdylib --features sim
# -> target/wasm32-unknown-unknown/release/esp_mod_rtest.wasm   (exports: memory, xip_handle)
wasm-opt -Oz -o mod_rtest.wasm target/wasm32-unknown-unknown/release/esp_mod_rtest.wasm   # optional
wasm-tools print mod_rtest.wasm | grep -E '\((export|import)'
```

The output stays under the module's own `target/` (git-ignored). `--crate-type cdylib` is needed because the
modules declare `rlib` only; `--features sim` supplies the panic handler and the global allocator of a `no_std`
library (the same feature `scripts/build-host-modules.sh` uses for `.so` files).

## Build for a slot

All modules of a runtime share one memory and each is linked at its slot base (slot N at N * 64 KiB, data then stack). A module
loaded twice needs a file per slot, so the host build makes eight of each:

```sh
scripts/build-wasm.sh mod_rtest 1      # one slot -> modules/mod_rtest/target/wasm-slot1/wasm32-unknown-unknown/release/esp_mod_rtest.wasm
scripts/build-host-modules.sh          # rtest, ble, system (mod_system: sys, hw), slots 1..8 -> target/host-modules/mod_<name>.s<N>.wasm (+ .json metadata)
```

## Run it

`crates/xip-wasm` is a library, not a runtime: it holds the shared memory and runs `xip_handle` of a wasm module against the
`Host` table of the runtime that uses it (`xip-runtime-host` today; the C runtime or the mmap loader could use the same bridge).
The host runtime loads `NAME.wasm` modules (`Runtime::add_module`, `load_module`); its tests and `xip-host` use them.

```sh
scripts/build-host-modules.sh
cargo test --manifest-path crates/xip-runtime-host/Cargo.toml                 # the host runtime's tests, modules as wasm
cargo build --manifest-path crates/xip-runtime-host/Cargo.toml --target x86_64-unknown-linux-musl   # a static binary
XIP_WASM_TRACE=1 cargo test ...                                               # names the host calls the wasm bridge lacks
```

`xip-abi` has a `wasm32` mode: there is no host table. The module reaches the host through named imports of the module `xip`
(`xip-abi/src/lib.rs`, mod `wasm`; the bridge is in `xip-wasm/src/lib.rs`): the plain table calls (log, pools, packets,
settings, alarms, sockets, modules, nv, flash, heap, device info) and the `hw` component (gpio, irq, spi, i2c, adc, led). Pointers
are offsets into the one shared memory. The runtime builds its packet pool and each module's heap inside regions of that memory
(`Pool::attach_region`, `Mem::with_arena`).

## Direction

- Per-slot link bases (done; later PIC with a runtime base, as a nightly feature).
- Each module has its own slot(s), state block and heap; the only memory modules share is packets.
- TODO (not critical, only host/Android): check that a module touches only its own slot, heap and the packets it holds.
- Calls between modules go through the component vtables (below) or packets by `(component, method)`.
- Ahead-of-time compilation to riscv, xtensa, amd64 and arm later; the mmap loader shares the slot arithmetic.

Differences from the C and host runtimes are tracked in [WASM-PORTING.md](WASM-PORTING.md).

## Status

`xip-runtime-host` loads its modules as wasm (`libloading` and `.so` are gone, the crate links as a static musl binary) and **all 110 of
its tests pass** with the modules loaded from wasm: every `mod_rtest` mode, `mod_system`, the sys component's envelopes, the component tables
(`wifi`, `wifi-tuning`, `link`, `power`, `nimble`) and the `ble` table that `mod_ble` hosts. `socket_readiness_native` and
`lwip_poll_native` (C files missing from this tree) were replaced by `tests/socket_readiness.rs` (tokio sockets). `stream` and `quic`
tables are not bridged yet (no host test uses them from a wasm module).

The other modules (`hw ble ... lora nan disc flash quic`) need a `sim` feature before they build as wasm; `mod_nan` needs the `dmesh`
checkout path patch (`dmesh-rawnan`).
