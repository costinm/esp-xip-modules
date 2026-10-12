#!/usr/bin/env bash
# Sign the built firmware and collect everything the rolling release `latest` carries into REL (default target/rel).
#   EM_SIGNING_KEY=<PEM text>  scripts/publish-release.sh            # CI: the key from a secret
#   scripts/publish-release.sh KEY.pem                               # locally
# Needs target/out/<cpu>/{main.bin,modules/*.dmod} (scripts/build.sh).
# The key must be the one whose public half is keys/modules.pub (or EM_PUBKEY=FILE): a device pins the key of the first main
# it installs (trust on first use) and only accepts that key's modules afterwards. Prints version=<yymmddHHMM> (also written to $GITHUB_ENV).
#
# CHIPS="esp32c6 esp32s3" limits it to those chips.
# Output, per chip:
#   <cpu>.wlpkg            main, signed for esp-wifi-loader (installed over Wi-Fi by the loader's flasher)
#   <cpu>-<module>.wlpkg   each module, signed for the module upload port of a running main
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REL="${REL:-$ROOT/target/rel}"
SIGN="${SIGN:-$(command -v esp-wifi-sign || true)}"
SIGN="${SIGN:-${ESP_WIFI_LOADER_DIR:-$ROOT/../esp-wifi-loader}/tools/sign-bundle.py}"
SINGLE="${SIGN_ONE:-$(command -v esp-wifi-sign-one || true)}"
SINGLE="${SINGLE:-${ESP_WIFI_LOADER_DIR:-$ROOT/../esp-wifi-loader}/tools/sign.py}"
if [[ $SIGN == *.py ]]; then SIGN=(python3 "$SIGN"); else SIGN=("$SIGN"); fi
if [[ $SINGLE == *.py ]]; then SINGLE=(python3 "$SINGLE"); else SINGLE=("$SINGLE"); fi
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
key="${1:-}"
if [[ -z $key ]]; then
    [[ -n ${EM_SIGNING_KEY:-} ]] || { echo "::error::secret EM_SIGNING_KEY is not set (docs/releasing.md)"; exit 1; }
    key="$tmp/modules-key.pem"; install -m 600 /dev/null "$key"; printf '%s\n' "$EM_SIGNING_KEY" > "$key"
fi
[[ "$("${SINGLE[@]}" pubkey "$key")" == "$(cat "${EM_PUBKEY:-$ROOT/keys/modules.pub}")" ]] ||
    { echo "::error::the signing key does not match keys/modules.pub"; exit 1; }
version="${VERSION:-$(date -u +%y%m%d%H%M)}"      # monotonic: devices refuse older versions
# The Android flasher and the loader read the download source from a Main package: owner/repo[:tag]
source_repo="${EM_SOURCE:-${GITHUB_REPOSITORY:-costinm/esp-xip-modules}:latest}"
input="$ROOT/target/out"
if [[ -n ${CHIPS:-} ]]; then
    input="$tmp/input"; mkdir -p "$input"
    for cpu in $CHIPS; do
        [[ -d $ROOT/target/out/$cpu ]] || continue
        ln -s "$ROOT/target/out/$cpu" "$input/$cpu"
    done
fi
rm -rf "$REL"
"${SIGN[@]}" --input "$input" --key "$key" --output "$REL" --version "$((10#$version))" \
    --fqdn "$source_repo" --pubkey "${EM_PUBKEY:-$ROOT/keys/modules.pub}"
echo "version=$version"
[[ -z ${GITHUB_ENV:-} ]] || echo "version=$version" >> "$GITHUB_ENV"
