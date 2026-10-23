#!/usr/bin/env bash
# Build the DMesh firmware (fw/main) for one chip with the ESP Rust toolchain selected by this repository's env.sh.
#   fw/build.sh [esp32c6|esp32|esp32s3]       default esp32c6
# Output: target/fw/<cpu>/main-app.bin, the application image for the `main` partition (0xf0000).
# Environment: XIP_ESP_ROOT (ESP-IDF and Rust ESP installation, see env.sh); the DMesh libraries are Cargo git dependencies at
# a pinned revision (see Cargo.toml). Optional: DMESH_FW_FEATURES, DMESH_FW_NO_DEFAULT_FEATURES=1,
# DMESH_UART_BAUD (115200|230400|460800|921600), DMESH_BUILD_TIMESTAMP.
#   BLE is on by default for the C6 and off for ESP32 and ESP32-S3 (as scripts/build-fw.sh of DMesh did).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
XIP_REPO="$(cd "$HERE/.." && pwd)"
. "$XIP_REPO/env.sh" >/dev/null
[ -n "${XIP_RUST_ESP_BIN:-}" ] && [ -x "$XIP_RUST_ESP_BIN/cargo" ] || {
    echo "ESP Rust toolchain unavailable; set XIP_ESP_ROOT to an installation with rustup/toolchains/esp" >&2; exit 1;
}
export PATH="$XIP_RUST_ESP_BIN:$PATH"
for clang_lib in "$XIP_ESP_ROOT"/rustup/toolchains/esp/xtensa-esp32-elf-clang/*/esp-clang/lib/libclang.so; do
    if [ -e "$clang_lib" ]; then
        export LIBCLANG_PATH="${clang_lib%/libclang.so}"
        break
    fi
done
if [ -z "${LIBCLANG_PATH:-}" ] || [ ! -e "$LIBCLANG_PATH/libclang.so" ]; then
    echo "Espressif libclang missing under $XIP_ESP_ROOT; run scripts/esp-deps.sh" >&2
    exit 1
fi
# Reuse the selected SDK tool installation; embuild must not bootstrap another SDK Python environment.
export ESP_IDF_TOOLS_INSTALL_DIR=fromenv
export CARGO_TARGET_DIR="${XIP_FW_TARGET_DIR:-$XIP_REPO/target/fw}"
cpu="${1:-esp32c6}"
no_defaults="${DMESH_FW_NO_DEFAULT_FEATURES:-0}"
features="${DMESH_FW_FEATURES:-}"
case "$cpu" in
    esp32c6) target=riscv32imac-esp-espidf; sdkconfig=sdkconfig.esp32c6.defaults ;;
    esp32) target=xtensa-esp32-espidf; sdkconfig=sdkconfig.defaults ;;
    esp32s3) target=xtensa-esp32s3-espidf; sdkconfig=sdkconfig.esp32s3.defaults ;;
    *) echo "usage: fw/build.sh [esp32c6|esp32|esp32s3]" >&2; exit 2 ;;
esac
if [ "$cpu" != esp32c6 ] && [ -z "${DMESH_FW_NO_DEFAULT_FEATURES+x}" ] && [ -z "${DMESH_FW_FEATURES+x}" ]; then
    no_defaults=1; features=modules
fi
args=(build --release --target "$target")
[ "$no_defaults" = 1 ] && args+=(--no-default-features)
[ -n "$features" ] && args+=(--features "$features")

# sdkconfig overlay: the partition table by absolute path (ESP-IDF resolves a relative one inside its generated project), the flash
# settings of the image (Main's driver is always DIO; Stage2 selects QIO code reads), and no Bluetooth when the BLE adapter is not linked.
mkdir -p "$CARGO_TARGET_DIR/sdkconfig"
overlay="$CARGO_TARGET_DIR/sdkconfig/$cpu-overlay.defaults"
{
    printf 'CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="%s"\n' "$HERE/main/partitions.csv"
    printf 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y\nCONFIG_ESPTOOLPY_FLASHMODE_QIO=n\nCONFIG_ESPTOOLPY_FLASHMODE="dio"\n'
    printf 'CONFIG_ESPTOOLPY_FLASHFREQ_40M=n\nCONFIG_ESPTOOLPY_FLASHFREQ_80M=y\nCONFIG_ESPTOOLPY_FLASHFREQ="80m"\n'
    if [ "$no_defaults" = 1 ] && [[ ",$features," != *",ble-stack,"* ]]; then
        printf 'CONFIG_BT_ENABLED=n\nCONFIG_BT_NIMBLE_ENABLED=n\n'
    fi
} > "$overlay.tmp"
# a changed file would make esp-idf-sys rebuild the SDK: replace it only when the contents differ
if cmp -s "$overlay.tmp" "$overlay"; then rm -f "$overlay.tmp"; else mv -f "$overlay.tmp" "$overlay"; fi

# The native components are outside Cargo's graph: when their sources or the sdkconfig change, drop the esp-idf-sys build so
# the SDK is configured again.
stamp="$CARGO_TARGET_DIR/.fw-sdk-$cpu"
digest="$( { sha256sum "$HERE/main/$sdkconfig" "$HERE/main/sdkconfig.defaults" "$overlay" "$HERE/main/Cargo.toml" "$HERE/main/partitions.csv";
    find "$XIP_REPO/components" "$HERE/ble/native" "$HERE/main/native" -type f \( -name '*.c' -o -name '*.h' -o -name CMakeLists.txt -o -name Kconfig \) | sort | xargs sha256sum; } | sha256sum | cut -d' ' -f1)"
digest="$digest:${IDF_PATH:-}:$no_defaults:$features"
if [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$digest" ]; then
    echo "SDK configuration or native components changed; clearing the esp-idf-sys build for $target" >&2
    rm -rf "$CARGO_TARGET_DIR/$target/release/build"/esp-idf-sys-* "$CARGO_TARGET_DIR/$target/release/.fingerprint"/esp-idf-sys-*
    printf '%s' "$digest" > "$stamp"
fi

cd "$HERE/main"
export ESP_IDF_SDKCONFIG_DEFAULTS="$sdkconfig;$overlay"
cargo "${args[@]}"
out="$CARGO_TARGET_DIR/$cpu"
mkdir -p "$out"
python3 -m esptool --chip "$cpu" elf2image --flash-mode dio --flash-freq 80m --flash-size 4MB \
    --output "$out/main-app.bin" "$CARGO_TARGET_DIR/$target/release/dmesh-rs"
printf 'MAIN_IMAGE=%s MAIN_IMAGE_SIZE=%s MAIN_IMAGE_SHA256=%s\n' "$out/main-app.bin" "$(stat -c %s "$out/main-app.bin")" \
    "$(sha256sum "$out/main-app.bin" | cut -d' ' -f1)"
