#!/usr/bin/env bash
# Build module(s) as wasm for a slot: modules/build-wasm.sh mod_rtest 1 [cargo features]
# The module imports the shared memory and is linked at the slot's base (xip-wasm: slot * 64 KiB), stack after data.
# Output: modules/<mod>/target/wasm-slot<N>/wasm32-unknown-unknown/release/*.wasm.  Needs the wasm-dev tools on PATH (modules/WASM.md).
set -euo pipefail
mod=$1; slot=$2; features=${3:-sim}
cd "$(dirname "$0")/$mod"
cargo rustc --quiet --release --lib --target wasm32-unknown-unknown --crate-type cdylib --features "$features" \
    --target-dir target/wasm-slot$slot -- \
    -C link-arg=--import-memory -C link-arg=--export-table -C link-arg=--no-stack-first \
    -C link-arg=--global-base=$((slot * 65536)) -C link-arg=-zstack-size=8192
