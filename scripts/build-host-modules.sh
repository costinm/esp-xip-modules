#!/usr/bin/env bash
# Build the Rust modules for the development host as shared objects (target/host-modules/<name>.so) that the xip-runtime-host loads:
# the same sources that become flash images, compiled for this machine.   scripts/build-host-modules.sh
set -euo pipefail
cd "$(dirname "$0")/.."
out=target/host-modules
mkdir -p $out
# the `sim` feature adds the panic handler of a no_std library; cdylib makes the shared object
for m in rtest con; do
    RUSTC_BOOTSTRAP=1 cargo rustc --quiet --release --lib --crate-type cdylib --features sim \
        --manifest-path modules/mod_$m/Cargo.toml --target-dir target/host-modules-build/$m
    cp target/host-modules-build/$m/release/libesp_mod_$m.so $out/mod_$m.so
done
