#!/usr/bin/env bash
# Build the Rust example main for one chip with the ESP Rust toolchain selected by this repository's env.sh.
#   build.sh [esp32c6|esp32|esp32s3]        default esp32c6; ELF and .bin under target/example/<target>/release/
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EM_REPO="$(cd "$HERE/../../.." && pwd)"
. "$EM_REPO/env.sh" >/dev/null
[ -n "${EM_RUST_ESP_BIN:-}" ] && [ -x "$EM_RUST_ESP_BIN/cargo" ] || {
    echo "ESP Rust toolchain unavailable; set EM_ESP_ROOT to an installation with rustup/toolchains/esp" >&2; exit 1;
}
export PATH="$EM_RUST_ESP_BIN:$PATH"
for clang_lib in "$EM_ESP_ROOT"/rustup/toolchains/esp/xtensa-esp32-elf-clang/*/esp-clang/lib/libclang.so; do
    if [ -e "$clang_lib" ]; then
        export LIBCLANG_PATH="${clang_lib%/libclang.so}"
        break
    fi
done
if [ -z "${LIBCLANG_PATH:-}" ] || [ ! -e "$LIBCLANG_PATH/libclang.so" ]; then
    echo "Espressif libclang missing under $EM_ESP_ROOT; run scripts/esp-deps.sh" >&2
    exit 1
fi
# Reuse the selected SDK tool installation; embuild must not bootstrap another
# SDK Python environment inside the example source tree.
export ESP_IDF_TOOLS_INSTALL_DIR=fromenv
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
cargo build --release --target "$target"
python3 -m esptool --chip "${1:-esp32c6}" elf2image --flash-mode dio --flash-freq 80m \
    --flash-size 4MB --output "$CARGO_TARGET_DIR/$target/release/espmod-main.bin" \
    "$CARGO_TARGET_DIR/$target/release/espmod-main"
