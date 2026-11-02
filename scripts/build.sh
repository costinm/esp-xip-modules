#!/usr/bin/env bash
# Build the runtime image (the C main: an example, and the host for the modules) and the Rust modules.   . ./env.sh && scripts/build.sh [esp32|esp32s3|esp32c6|all]
# XIP_FLASH_MB=<4|8|16> selects the partition table (boot/partitions/<n>mb.csv) and the flash size setting
# of Main and Stage2, for boards with that much flash. Default: the 4 MB table and the per-chip Stage2 size.
# For the S3 16 MB build it also enables the 8 MB octal PSRAM (XIP_PSRAM=0 opts out).
# Output:
#   target/out/<cpu>/main.bin             the runtime image (BLE standard by default); it goes to the start of the xip partition (0x10000)
#   target/out/<cpu>/stage2-4mb.bin, partition-table-4mb.bin   Stage2 bootloader
#       and partition table (-<n>mb with XIP_FLASH_MB), built from boot/
#   target/out/<cpu>/modules/<name>.dmod  module images; modules.txt: name, page offset to flash it at, slot
# Nothing is signed or keyed here.
set -euo pipefail
cd "$(dirname "$0")/.."
cpus="${1:-all}"
[[ $cpus == all ]] && cpus="esp32 esp32s3 esp32c6"
table=4mb; flashsize=""
case "${XIP_FLASH_MB:-}" in
    "") ;;
    4|8|16) table=${XIP_FLASH_MB}mb; flashsize=CONFIG_ESPTOOLPY_FLASHSIZE_${XIP_FLASH_MB}MB=y;;
    *) echo "XIP_FLASH_MB: 4, 8 or 16 (got ${XIP_FLASH_MB})" >&2; exit 2;;
esac
[[ -f boot/partitions/$table.csv ]] || { echo "no partition table: boot/partitions/$table.csv" >&2; exit 2; }
[[ -n $flashsize ]] && { mkdir -p target/out; echo "$flashsize" > "target/out/flashsize-$table.defaults"; }
# Octal PSRAM on the 16 MB S3 lab modules (N16R8, 8 MB AP3406). The 8 MB S3 boards have none,
# so the setting follows the table; XIP_PSRAM=0 opts out. IGNORE_NOTFOUND keeps the image
# bootable on a no-PSRAM board (Wi-Fi falls back to the no-PSRAM buffer config).
if [[ $table == 16mb ]]; then
    mkdir -p target/out
    printf 'CONFIG_SPIRAM=y\nCONFIG_SPIRAM_MODE_OCT=y\nCONFIG_SPIRAM_TYPE_AUTO=y\nCONFIG_SPIRAM_SPEED_80M=y\nCONFIG_SPIRAM_IGNORE_NOTFOUND=y\n' > "target/out/psram-$table.defaults"
fi

for cpu in $cpus; do
    out=target/out/$cpu
    mkdir -p $out/modules
    defaults="$PWD/sdkconfig.defaults"
    [[ -f sdkconfig.$cpu.defaults ]] && defaults="$defaults;$PWD/sdkconfig.$cpu.defaults"
    # The flash size of the board (XIP_FLASH_MB): the runtime caps the addressable flash at its configured size.
    [[ -n $flashsize ]] && defaults="$defaults;$PWD/target/out/flashsize-$table.defaults"
    # Octal PSRAM on the 16 MB S3 modules (XIP_PSRAM=0 opts out).
    [[ $cpu == esp32s3 && $table == 16mb && ${XIP_PSRAM:-1} != 0 ]] && defaults="$defaults;$PWD/target/out/psram-$table.defaults"
    main_profile=${XIP_BLE_PROFILE:-standard}
    [[ $main_profile == */* ]] && profile=$main_profile || profile=$PWD/components/xip_ble/profiles/$main_profile.defaults
    [[ -f $profile ]] || { echo "missing BLE profile: $profile" >&2; exit 1; }
    # The version of the image: a number that rises with every build, seconds since 2025-01-01 unless XIP_RUNTIME_VERSION says otherwise. It is
    # the app description's version string (nothing reads it yet; the extent scan will pick the highest).
    version=${XIP_RUNTIME_VERSION:-$(( $(date +%s) - 1735689600 ))}
    [[ $version =~ ^[0-9]+$ ]] || { echo "XIP_RUNTIME_VERSION must be a number (got $version)" >&2; exit 2; }
    rm -f "$out/main.sdkconfig"
    IDF_TARGET=$cpu idf.py -B "$out/main-build" -D SDKCONFIG="$PWD/$out/main.sdkconfig" -D PROJECT_VER="$version" \
        -D SDKCONFIG_DEFAULTS="$defaults;$profile" build
    grep -qxF 'CONFIG_ESPTOOLPY_FLASHMODE_DIO=y' "$out/main.sdkconfig" || { echo "$cpu main: flash driver must be DIO" >&2; exit 1; }
    cp "$out/main-build/esp-xip-modules.bin" "$out/main.bin"
    # Stage2 (boot/, see boot/README.md): defaults of the chip with the selected partition table;
    # XIP_FLASH_MB also raises the flash size setting (the chip exposes only its configured size).
    cfg=$out/stage2-$table.sdkconfig
    { if [[ $cpu == esp32 ]]; then cat boot/sdkconfig.defaults; else cat boot/sdkconfig.$cpu.defaults; fi
      if [[ -n $flashsize ]]; then
          echo "$flashsize"
          echo "CONFIG_PARTITION_TABLE_CUSTOM_FILENAME=\"partitions/$table.csv\""
      fi
    } > $cfg
    rm -f $out/stage2-$table.sdkconfig.gen
    IDF_TARGET=$cpu idf.py -C boot -B $out/stage2-build -D SDKCONFIG=$PWD/$out/stage2-$table.sdkconfig.gen -D SDKCONFIG_DEFAULTS=$PWD/$cfg build
    cp $out/stage2-build/bootloader/bootloader.bin $out/stage2-$table.bin
    cp $out/stage2-build/partition_table/partition-table.bin $out/partition-table-$table.bin
    # the modules: the loader scans every 64 KB page of xip after the runtime reserve (1.5 MiB), so an image goes on any of them. A module has a
    # slot (modules/SLOTS: its RAM cell and, on the Xtensa chips, its link window), which is NOT its page.
    # modules.txt: name, the flash address esptool writes it at (the pages after the reserve, modules/CRITICAL first), the slot.
    # XIP_MODULE_VERSIONS="mod_x=3 mod_y=2" builds those modules with that manifest version (default 0, no key).
    : > $out/modules.list
    for dir in modules/mod_*/; do
        mod=$(basename $dir)
        version=0
        for kv in ${XIP_MODULE_VERSIONS:-}; do [[ ${kv%%=*} == $mod ]] && version=${kv#*=}; done
        if [ -f $dir/Cargo.toml ]; then
            python3 tools/build_mod.py $dir --chip $cpu --out $out/modules --version $version
        else
            rc=0
            python3 tools/build_c_mod.py $dir --chip $cpu --out $out/modules/$mod.dmod --version $version || rc=$?
            # exit 3: the module declares other chips (modules/C-MODULES); it takes no page here.
            if [ $rc == 3 ]; then continue; fi
            [ $rc == 0 ] || exit $rc
        fi
        echo "$mod $(python3 -c "import sys; sys.path.insert(0, 'tools'); import mkmod; print(mkmod.read_slots()['$mod'])")" >> $out/modules.list
    done
    # Place the images on actual rounded extents, multi-page modules included, in the exact table built above; never emit a layout
    # beyond the board's flash. Manual flash: runtime at 0x10000, modules at their addresses.
    python3 tools/partition_layout.py "$out/partition-table-$table.bin" --main-image "$out/main.bin" --modules "$out/modules.list" \
        --images "$out/modules" --critical modules/CRITICAL --out "$out/modules.txt" >/dev/null
done
(cd target/out && find . -name '*.bin' -o -name '*.dmod' | sort | xargs sha256sum > SHA256SUMS)
