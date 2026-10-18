#!/usr/bin/env bash
# Build the runtime image (the C main: an example, and the host for the modules) and the Rust modules.   . ./env.sh && scripts/build.sh [esp32|esp32s3|esp32c6|all]
# Output:
#   target/out/<cpu>/main.bin             the runtime image (sign it: docs/releasing.md)
#   target/out/<cpu>/modules/<name>.dmod  unsigned module images; modules.txt: the page (offset in the module partition) each was built for
# Nothing here is signed or keyed: sign the results with your own key (docs/releasing.md); a device pins the key of the first package it installs.
set -euo pipefail
cd "$(dirname "$0")/.."
cpus="${1:-all}"
[[ $cpus == all ]] && cpus="esp32 esp32s3 esp32c6"

for cpu in $cpus; do
    out=target/out/$cpu
    mkdir -p $out/modules
    defaults="$PWD/sdkconfig.defaults"
    [[ -f sdkconfig.$cpu.defaults ]] && defaults="$defaults;$PWD/sdkconfig.$cpu.defaults"
    rm -f $out/main.sdkconfig
    IDF_TARGET=$cpu idf.py -B $out/main-build -D SDKCONFIG=$PWD/$out/main.sdkconfig -D SDKCONFIG_DEFAULTS="$defaults" build
    # Stage2 boots Main in DIO or QIO; the driver must stay DIO (esp-wifi-loader docs/app-integration.md).
    grep -qxF 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y' $out/main.sdkconfig || { echo "$cpu: flash driver must be DIO" >&2; exit 1; }
    cp $out/main-build/esp-xip-modules.bin $out/main.bin
    # the modules: the loader scans the module partition, so an image goes on any 64 KB page. The Xtensa chips link an image for one address, which
    # is its page: the build gives each module the next one, and modules.txt says where (C6: the same order, any page would do).
    offset=0
    : > $out/modules.txt
    for dir in modules/mod_*/; do
        python3 tools/build_mod.py $dir --chip $cpu --offset $offset --out $out/modules
        echo "$(basename $dir) $offset" >> $out/modules.txt
        offset=$((offset + 65536))
    done
done
(cd target/out && find . -name '*.bin' -o -name '*.dmod' | sort | xargs sha256sum > SHA256SUMS)
