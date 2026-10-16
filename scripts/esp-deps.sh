#!/usr/bin/env bash
# Install ESP-IDF v6.0.2 and the esp32/esp32s3/esp32c6 toolchains under target/,
# and the host tools into a repo-local Nix profile (flake.nix).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$REPO_ROOT/env.sh"
ESP_ROOT="$XIP_ESP_ROOT"
PROFILE="${XIP_NIX_PROFILE:-$REPO_ROOT/target/nix/profile}"
IDF_VERSION="v6.0.2"
JOBS="${ESP_IDF_GIT_JOBS:-8}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export IDF_TOOLS_PATH="$ESP_ROOT/espressif"
mkdir -p "$ESP_ROOT" "$IDF_TOOLS_PATH" "$(dirname "$PROFILE")"

if command -v nix >/dev/null 2>&1; then
    # `path:` includes untracked files, so this works before the first commit.
    if [[ -e "$PROFILE" ]]; then
        nix profile upgrade --profile "$PROFILE" deps ||
            nix profile add --profile "$PROFILE" "path:$REPO_ROOT#deps"
    else
        nix profile add --profile "$PROFILE" "path:$REPO_ROOT#deps"
    fi
    if [[ ! -x "$PROFILE/bin/espup" || ! -x "$PROFILE/bin/ldproxy" ]]; then
        nix profile add --profile "$PROFILE" "path:$REPO_ROOT#deps"
    fi
elif [[ ! -x "$PROFILE/bin/cmake" ]]; then
    echo "nix is unavailable and $PROFILE has no host tools" >&2
    exit 127
fi
if [[ ! -x "$PROFILE/bin/espup" || ! -x "$PROFILE/bin/ldproxy" ]]; then
    echo "incomplete host profile: $PROFILE needs espup and ldproxy" >&2
    exit 1
fi
export PATH="$PROFILE/bin:$PATH"
if [[ ${1:-} == host ]]; then
    echo "Host profile ready: $PROFILE"
    exit 0
fi

if [[ ! -d "$ESP_ROOT/esp-idf/.git" ]]; then
    git clone --branch "$IDF_VERSION" --depth 1 --recursive --jobs "$JOBS" \
        https://github.com/espressif/esp-idf.git "$ESP_ROOT/esp-idf"
else
    git -C "$ESP_ROOT/esp-idf" checkout --force "$IDF_VERSION"
    git -C "$ESP_ROOT/esp-idf" submodule sync --recursive
    git -C "$ESP_ROOT/esp-idf" submodule update --init --recursive --force --jobs "$JOBS"
fi
"$ESP_ROOT/esp-idf/install.sh" esp32,esp32s3,esp32c6
if [[ ! -x "$ESP_ROOT/rustup/toolchains/esp/bin/rustc" ]]; then
    export RUSTUP_HOME="$ESP_ROOT/rustup"
    export CARGO_HOME="$ESP_ROOT/cargo"
    ESP_HOME="$ESP_ROOT/home"
    mkdir -p "$ESP_HOME" "$RUSTUP_HOME" "$CARGO_HOME"
    HOME="$ESP_HOME" espup install --targets esp32,esp32s3,esp32c6 --export-file "$ESP_ROOT/export-esp.sh"
fi
if ! compgen -G "$ESP_ROOT/rustup/toolchains/esp/xtensa-esp32-elf-clang/*/esp-clang/lib/libclang.so" >/dev/null; then
    echo "Espressif libclang missing after espup install" >&2
    exit 1
fi
echo "ESP-IDF $IDF_VERSION ready. Load with: . env.sh"
