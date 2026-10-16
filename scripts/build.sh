#!/usr/bin/env bash
# Build main and the modules. Uses idf.py from the PATH, else ./env.sh (scripts/esp-deps.sh installs it);
# in the container it is already set up.
#   scripts/build.sh [esp32|esp32s3|esp32c6|all] [main|loader]
# One runtime/configuration; main|loader selects the destination size check only.
# The modules use the shared product slot table in crates/xip-runtime-esp/src/loader.rs. Output:
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
slots="$ROOT/crates/xip-runtime-esp/src/loader.rs"
[[ $CPUS == all ]] && CPUS="esp32 esp32s3 esp32c6"

for cpu in $CPUS; do
    out="$ROOT/target/out/$cpu"
    mkdir -p "$out/modules"
    python3 "$ROOT/tools/gen_slots.py" "$slots" "$out/product-slots.inc"
    defaults="$ROOT/sdkconfig.defaults"
    [[ -f $ROOT/sdkconfig.$cpu.defaults ]] && defaults="$defaults;$ROOT/sdkconfig.$cpu.defaults"
    rm -f "$out/main.sdkconfig"
    IDF_TARGET=$cpu idf.py -C "$ROOT" -B "$out/main-build" \
        -D SDKCONFIG="$out/main.sdkconfig" -D SDKCONFIG_DEFAULTS="$defaults" -D XIP_PRODUCT_INCLUDE="$out" build
    # Stage2 boots Main in DIO or QIO; the driver must stay DIO (esp-wifi-loader docs/app-integration.md).
    grep -qxF 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y' "$out/main.sdkconfig" ||
        { echo "$cpu: flash driver must be DIO" >&2; exit 1; }
    image="$out/main-build/esp-xip-modules.bin"
    # Destination is a packaging choice; it does not alter the runtime.
    # Use the SDK parser so hexadecimal sizes and K/M suffixes stay consistent.
    python3 - "$IDF_PATH" "$ROOT/partitions.csv" "$ROLE" "$image" "$out" <<'PY_CHECK'
import pathlib, shutil, sys
sys.path.insert(0, str(pathlib.Path(sys.argv[1]) / 'components/partition_table'))
from gen_esp32part import PartitionTable
table = PartitionTable.from_csv(pathlib.Path(sys.argv[2]).read_text())
role, image, out = sys.argv[3], pathlib.Path(sys.argv[4]), pathlib.Path(sys.argv[5])
part = next(p for p in table if p.name == role)
capacity = part.size - (8192 if role == 'loader' else 0)
size = image.stat().st_size
if size > capacity:
    raise SystemExit(f'{role}: image {size} bytes exceeds usable capacity {capacity}')
print(f'{role}: image {size} bytes fits at {part.offset:#x}; usable capacity {capacity}')
for name in ['runtime.bin', 'main.bin']:
    shutil.copyfile(image, out / name)
loader = next(p for p in table if p.name == 'loader')
legacy = out / 'c-loader/loader.bin'
if size <= loader.size - 8192:
    legacy.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(image, legacy)
else:
    legacy.unlink(missing_ok=True)
PY_CHECK

    # every module: modules/mod_<name>/ with the sources of the slot <name>. C modules: the .c files, options in a
    # comment (/* mkmod: --state 64 --stack 4096 */). Rust modules: a Cargo.toml (bin mod_<name>, feature flat-image),
    # options in [package.metadata] mkmod = "...". Modules absent from the product table are skipped.
    for dir in "$ROOT"/modules/mod_*/; do
        name="$(basename "$dir")"; slot="${name#mod_}"
        # a module may name the slot it shares with another (mod_rtest and mod_test: the one `test` slot): --slot in its options
        if [[ -f $dir/Cargo.toml ]]; then opts="$(sed -n 's|^mkmod = "\(.*\)"|\1|p' "$dir/Cargo.toml" | head -1)"
        else opts="$(sed -n 's|.*mkmod: \(.*\) \*/.*|\1|p' "$dir"*.c | head -1)"; fi
        [[ $opts =~ --slot[[:space:]]+([a-z0-9_]+) ]] && { slot="${BASH_REMATCH[1]}"; opts="${opts/--slot ${slot}/}"; }
        python3 "$ROOT/tools/mkmod.py" --chip "$cpu" --slots "$slots" --list-slots | grep -q "^$slot:" || { echo "$name: no slot, not packed"; continue; }
        if [[ -f $dir/Cargo.toml ]]; then
            if [[ -z ${XIP_RUST_ESP_BIN:-} && -z ${XIP_RUST_MODULES:-} ]]; then
                echo "$name: Rust module skipped (needs the esp toolchain: . ./env.sh finds it in an ESP-IDF install made with DMesh, or set XIP_RUST_MODULES=1 when cargo has it)"
                continue
            fi
            "$ROOT/scripts/build-mod.sh" "$dir" --chip "$cpu" --slots "$slots" --slot "$slot" --out "$out/modules"
        else
            "$ROOT/scripts/build-mod.sh" "$dir" --chip "$cpu" --slots "$slots" --slot "$slot" --out "$out/modules"
        fi
    done
    # Existing deployment callers use this path; modules have one shared build.
    mkdir -p "$out/c-loader/modules"
    cp "$out/modules/"*.dmod "$out/c-loader/modules/"
done
(cd "$ROOT/target/out" && find . -name '*.bin' -o -name '*.dmod' | sort | xargs sha256sum > SHA256SUMS)
