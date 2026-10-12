#!/usr/bin/env bash
# Build the Rust example main for one chip with the ESP Rust toolchain (the one DMesh's env.sh installs).
#   build.sh [esp32c6|esp32|esp32s3]        default esp32c6; the ELF is target/example/<target>/release/espmod-main
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EM_REPO="$(cd "$HERE/../../.." && pwd)"
. "$EM_REPO/env.sh" >/dev/null
DMESH="${EM_DMESH_REPO:-${EM_DMESH_DIR:-$EM_REPO/../dmesh}}"
[ -f "$DMESH/env.sh" ] || { echo "the ESP Rust toolchain comes from a DMesh checkout (EM_DMESH_DIR)" >&2; exit 1; }
. "$DMESH/env.sh" >/dev/null
export PATH="$RUST_ESP_TOOLCHAIN_BIN:$CARGO_HOME/bin:$PATH"
export CARGO_TARGET_DIR="${EM_EXAMPLE_TARGET_DIR:-$EM_REPO/target/example}"
case "${1:-esp32c6}" in
    esp32c6) target=riscv32imac-esp-espidf ;;
    esp32) target=xtensa-esp32-espidf ;;
    esp32s3) target=xtensa-esp32s3-espidf ;;
    *) echo "usage: build.sh [esp32c6|esp32|esp32s3]" >&2; exit 2 ;;
esac
# the partition table by absolute path: ESP-IDF resolves a relative one inside its generated project
mkdir -p "$CARGO_TARGET_DIR"
printf 'CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="%s"\n' "$HERE/partitions.csv" > "$CARGO_TARGET_DIR/partition.defaults"
export ESP_IDF_SDKCONFIG_DEFAULTS="$HERE/sdkconfig.defaults;$CARGO_TARGET_DIR/partition.defaults"
cd "$HERE"
exec cargo build --release --target "$target"
