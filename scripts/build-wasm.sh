#!/usr/bin/env bash
# Build module(s) as wasm for a slot: scripts/build-wasm.sh mod_rtest 1 [cargo features]
# The module imports the shared memory and is linked at the slot's base (xip-wasm: slot * 64 KiB), stack after data.
# Output: modules/<mod>/target/wasm-slot<N>/wasm32-unknown-unknown/release/*.wasm.  Needs the wasm-dev tools on PATH (modules/mod_wasm3/WASM.md).
set -euo pipefail
mod=$1; slot=$2; features=${3:-sim}
cd "$(dirname "$0")/../modules/$mod"
# Honor the module's declared stack, retaining the existing 8 KiB host minimum.
stack=$(python3 - <<'PY_STACK'
import shlex, tomllib
with open('Cargo.toml', 'rb') as f:
    args=shlex.split(tomllib.load(f).get('package', {}).get('metadata', {}).get('mkmod', ''))
print(max(8192, int(args[args.index('--stack')+1]) if '--stack' in args else 8192))
PY_STACK
)
cargo rustc --quiet --release --lib --target wasm32-unknown-unknown --crate-type cdylib --features "$features" \
    --target-dir target/wasm-slot$slot -- \
    -C link-arg=--import-memory -C link-arg=--export-table -C link-arg=--no-stack-first \
    -C link-arg=--global-base=$((slot * 65536)) -C link-arg=-zstack-size=$stack
