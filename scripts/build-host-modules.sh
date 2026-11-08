#!/usr/bin/env bash
# Build the Rust modules for the development host as WebAssembly (target/host-modules/<name>.sN.wasm, one file per slot) that
# xip-runtime-host loads: the same sources that become flash images. Needs the wasm toolchain on PATH (modules/mod_wasm3/WASM.md).
#   scripts/build-host-modules.sh
set -euo pipefail
cd "$(dirname "$0")/.."
# the wasm-dev out-link of the flake (nix build .#wasm-dev --out-link $WASM_DEV, modules/mod_wasm3/WASM.md): nightly rustc with the wasm32 targets, first on
# PATH for this script only (the other Rust tests use the toolchain of env.sh)
WASM_DEV=${WASM_DEV:-$HOME/opt/wasm-dev}
[ -d "$WASM_DEV/bin" ] && PATH="$WASM_DEV/bin:$PATH"
out=target/host-modules
mkdir -p $out
slots=${XIP_WASM_SLOTS:-8}
# the `sim` feature adds the panic handler and the global allocator of a no_std library
modules=("$@")
if [ ${#modules[@]} -eq 0 ]; then modules=(rtest ble system quic disc flash); fi
for m in "${modules[@]}"; do
    [[ "$m" =~ ^[a-z0-9_]+$ && -f modules/mod_$m/Cargo.toml ]] || { echo "unknown module: $m" >&2; exit 2; }
    for s in $(seq 1 "$slots"); do
        scripts/build-wasm.sh mod_$m $s sim
        cp modules/mod_$m/target/wasm-slot$s/wasm32-unknown-unknown/release/esp_mod_$m.wasm $out/mod_$m.s$s.wasm
    done
    python3 tools/build_mod.py modules/mod_$m --host-metadata $out/mod_$m.json
done
