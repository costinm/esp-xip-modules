#!/usr/bin/env bash
# Clean-install or recover a device by erasing the xip partition and flashing Stage2,
# partition table, main-recovery.bin, mod_flash.dmod, and mod_quic2.dmod (preserving NVS/wlstate/phy_init).
# Usage: scripts/flash-clean.sh CHIP PORT [FLASH_MB]
set -euo pipefail

if [[ ${1:-} == --help || ${1:-} == -h || $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: scripts/flash-clean.sh <esp32|esp32s3|esp32c6> <PORT> [4|8|16]" >&2
    exit 2
fi

CHIP="$1"
PORT="$2"
FLASH_MB="${3:-${XIP_FLASH_MB:-4}}"
IMAGES="${XIP_IMAGES:-target/out/$CHIP}"

case "$CHIP" in esp32|esp32s3|esp32c6) ;; *) echo "unknown chip: $CHIP" >&2; exit 2 ;; esac
case "$FLASH_MB" in 4|8|16) ;; *) echo "invalid flash size: $FLASH_MB (expected 4, 8, or 16)" >&2; exit 2 ;; esac

BOOT=0x0
[[ "$CHIP" != esp32 ]] || BOOT=0x1000
XIP_LEN=$((FLASH_MB * 1024 * 1024 - 0x10000))

for f in \
    "$IMAGES/stage2-${FLASH_MB}mb.bin" \
    "$IMAGES/partition-table-${FLASH_MB}mb.bin" \
    "$IMAGES/main-recovery.bin" \
    "$IMAGES/modules/mod_flash.dmod" \
    "$IMAGES/modules/mod_quic2.dmod"; do
    [[ -f "$f" ]] || { echo "missing artifact: $f" >&2; exit 1; }
done

espflash board-info -c "$CHIP" -p "$PORT" --non-interactive
espflash erase-region -c "$CHIP" -p "$PORT" --non-interactive --after no-reset 0x10000 "$XIP_LEN"
espflash write-bin -c "$CHIP" -p "$PORT" --non-interactive --after no-reset "$BOOT" "$IMAGES/stage2-${FLASH_MB}mb.bin"
espflash write-bin -c "$CHIP" -p "$PORT" --non-interactive --after no-reset 0x8000 "$IMAGES/partition-table-${FLASH_MB}mb.bin"
espflash write-bin -c "$CHIP" -p "$PORT" --non-interactive --after no-reset 0x10000 "$IMAGES/main-recovery.bin"
espflash write-bin -c "$CHIP" -p "$PORT" --non-interactive --after no-reset 0x190000 "$IMAGES/modules/mod_flash.dmod"
espflash write-bin -c "$CHIP" -p "$PORT" --non-interactive --after hard-reset 0x1b0000 "$IMAGES/modules/mod_quic2.dmod"
