#!/usr/bin/env bash
# Build main and the modules. Uses idf.py from the PATH, else ./env.sh (scripts/esp-deps.sh installs it);
# in the container it is already set up.
#   scripts/build.sh [esp32|esp32s3|esp32c6|all]
# The modules are built for the slot table in main/main.c. Output:
#   target/out/<cpu>/main.bin                    the image `main` (sign it: docs/releasing.md)
#   target/out/<cpu>/modules/<name>.dmod         unsigned module images
# Nothing here is signed or keyed: sign the results with your own key (docs/releasing.md); a device pins the key of
# the first package it installs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ${1:-} == deps ]]; then
    exec "$ROOT/scripts/esp-deps.sh" host
fi
command -v idf.py >/dev/null 2>&1 || . "$ROOT/env.sh"
command -v idf.py >/dev/null 2>&1 || { echo "idf.py not found: run scripts/esp-deps.sh (or use the container)" >&2; exit 1; }
CPUS="${1:-all}"
ROLE="${2:-main}"
case "$ROLE" in main|loader);; *) echo 'usage: scripts/build.sh [cpu|all] [main|loader]' >&2; exit 2;; esac
slots="$ROOT/main/main.c"
[[ $ROLE == loader ]] && slots="$ROOT/crates/espmod-host/src/loader.rs"
[[ $CPUS == all ]] && CPUS="esp32 esp32s3 esp32c6"

for cpu in $CPUS; do
    out="$ROOT/target/out/$cpu"
[[ $ROLE == loader ]] && out="$out/c-loader"
mkdir -p "$out/modules"
if [[ $ROLE == loader ]]; then
    python3 "$ROOT/tools/gen_slots.py" "$slots" "$out/product-slots.inc"
fi
    defaults="$ROOT/sdkconfig.defaults"
    [[ -f $ROOT/sdkconfig.$cpu.defaults ]] && defaults="$defaults;$ROOT/sdkconfig.$cpu.defaults"
    [[ $ROLE == loader ]] && defaults="$defaults;$ROOT/sdkconfig.loader.defaults"
    rm -f "$out/main.sdkconfig"
    IDF_TARGET=$cpu idf.py -C "$ROOT" -B "$out/main-build" \
        -D SDKCONFIG="$out/main.sdkconfig" -D SDKCONFIG_DEFAULTS="$defaults" -D ESPMOD_PRODUCT_INCLUDE="$out" build
    # Stage2 boots Main in DIO or QIO; the driver must stay DIO (esp-wifi-loader docs/app-integration.md).
    grep -qxF 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y' "$out/main.sdkconfig" ||
        { echo "$cpu: flash driver must be DIO" >&2; exit 1; }
    cp "$out/main-build/esp-xip-modules.bin" "$out/$ROLE.bin"

    # every module: modules/mod_<name>/ with the sources of the slot <name>. C modules: the .c files, options in a
    # comment (/* mkmod: --state 64 --stack 4096 */). Rust modules: a Cargo.toml (bin mod_<name>, feature flat-image),
    # options in [package.metadata] mkmod = "...". A module without a slot in main_slots[] is linked into main
    # only.
    for dir in "$ROOT"/modules/mod_*/; do
        name="$(basename "$dir")"; slot="${name#mod_}"
        # a module may name the slot it shares with another (mod_rtest and mod_test: the one `test` slot): --slot in its options
        if [[ -f $dir/Cargo.toml ]]; then opts="$(sed -n 's|^mkmod = "\(.*\)"|\1|p' "$dir/Cargo.toml" | head -1)"
        else opts="$(sed -n 's|.*mkmod: \(.*\) \*/.*|\1|p' "$dir"*.c | head -1)"; fi
        [[ $opts =~ --slot[[:space:]]+([a-z0-9_]+) ]] && { slot="${BASH_REMATCH[1]}"; opts="${opts/--slot ${slot}/}"; }
        python3 "$ROOT/tools/mkmod.py" --chip "$cpu" --slots "$slots" --list-slots | grep -q "^$slot:" || { echo "$name: no slot, not packed"; continue; }
        if [[ -f $dir/Cargo.toml ]]; then
            if [[ -z ${EM_RUST_ESP_BIN:-} && -z ${EM_RUST_MODULES:-} ]]; then
                echo "$name: Rust module skipped (needs the esp toolchain: . ./env.sh finds it in an ESP-IDF install made with DMesh, or set EM_RUST_MODULES=1 when cargo has it)"
                continue
            fi
            "$ROOT/scripts/build-mod.sh" "$dir" --chip "$cpu" --slots "$slots" --slot "$slot" --out "$out/modules"
        else
            "$ROOT/scripts/build-mod.sh" "$dir" --chip "$cpu" --slots "$slots" --slot "$slot" --out "$out/modules"
        fi
    done
done
(cd "$ROOT/target/out" && find . -name '*.bin' -o -name '*.dmod' | sort | xargs sha256sum > SHA256SUMS)
