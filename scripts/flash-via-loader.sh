#!/usr/bin/env bash
# Install our Main or a module on a device over Wi-Fi with the TOOLS OF THE OLD LOADER PROJECT (esp-wifi-loader), which sign.
# This is the only script of this repository that needs that project (docs: notes/docs/flash-via-loader.md).
#   scripts/flash-via-loader.sh [options] main FILE.bin | module NAME FILE.dmod | loader FILE.bin | stage2 FILE.bin
#     --loader DIR    the checkout of esp-wifi-loader (or $ESP_WIFI_LOADER_DIR)
#     --key FILE      private key (PEM); default: a key created in target/flash-via-loader.pem (the first package pins its public key)
#     --host H [--port P]   where to send it (default port 5227); without --host the signed package is only written
#     --chip esp32c6|esp32|esp32s3   (default esp32c6)      --version N (default: seconds since 2025, always rising)
#     --dry           verify on the device, change nothing
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
loader="${ESP_WIFI_LOADER_DIR:-}"; key="$ROOT/target/flash-via-loader.pem"; chip=esp32c6; port=5227; host=""; dry=""
version=$(( $(date +%s) - 1735689600 ))
while [[ $# -gt 0 ]]; do
    case $1 in
        --loader) loader=$2; shift 2;; --key) key=$2; shift 2;; --host) host=$2; shift 2;; --port) port=$2; shift 2;;
        --chip) chip=$2; shift 2;; --version) version=$2; shift 2;; --dry) dry=--dry; shift;; *) break;;
    esac
done
[[ -n $loader && -f $loader/tools/sign.py ]] || { echo "need the esp-wifi-loader checkout: --loader DIR or ESP_WIFI_LOADER_DIR" >&2; exit 2; }
kind=${1:-}; name=(); [[ $kind == module ]] && { name=(--name "${2:?module NAME}"); shift; }
image=${2:?image file}
[[ $kind =~ ^(main|module|loader|stage2)$ ]] || { echo "kind: main | module NAME | loader | stage2" >&2; exit 2; }
mkdir -p "$ROOT/target"
[[ -f $key ]] || python3 "$loader/tools/sign.py" keygen "$key"
out="$ROOT/target/$(basename "$image").wlpkg"
python3 "$loader/tools/sign.py" sign --key "$key" --cpu "$chip" --version "$version" --target "$kind" "${name[@]}" --image "$image" --out "$out"
echo "package: $out"
[[ -z $host ]] || python3 "$loader/tools/upload.py" send "$out" --host "$host" --port "$port" $dry
