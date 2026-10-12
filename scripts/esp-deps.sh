#!/usr/bin/env bash
# Install ESP-IDF v6.0.2 and the esp32/esp32s3/esp32c6 toolchains under target/,
# and the host tools into a repo-local Nix profile (flake.nix).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Prefer an existing DMesh install: no second ESP-IDF download (env.sh logic).
. "$REPO_ROOT/env.sh"
ESP_ROOT="$EM_ESP_ROOT"
if [[ -n "${EM_DMESH_REPO:-}" ]]; then
    echo "Reusing ESP-IDF from $ESP_ROOT (DMesh); nothing to install."
    echo "Load with: . env.sh   (EM_DMESH_DIR=none to install a private copy)"
    exit 0
fi
PROFILE="${EM_NIX_PROFILE:-$REPO_ROOT/target/nix/profile}"
IDF_VERSION="v6.0.2"
JOBS="${ESP_IDF_GIT_JOBS:-8}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export IDF_TOOLS_PATH="$ESP_ROOT/espressif"
mkdir -p "$ESP_ROOT" "$IDF_TOOLS_PATH" "$(dirname "$PROFILE")"

if command -v nix >/dev/null 2>&1; then
    # `path:` includes untracked files, so this works before the first commit.
    nix profile install --profile "$PROFILE" "path:$REPO_ROOT#deps"
elif [[ ! -x "$PROFILE/bin/cmake" ]]; then
    echo "nix is unavailable and $PROFILE has no host tools" >&2
    exit 127
fi
export PATH="$PROFILE/bin:$PATH"

if [[ ! -d "$ESP_ROOT/esp-idf/.git" ]]; then
    git clone --branch "$IDF_VERSION" --depth 1 --recursive --jobs "$JOBS" \
        https://github.com/espressif/esp-idf.git "$ESP_ROOT/esp-idf"
else
    git -C "$ESP_ROOT/esp-idf" checkout --force "$IDF_VERSION"
    git -C "$ESP_ROOT/esp-idf" submodule sync --recursive
    git -C "$ESP_ROOT/esp-idf" submodule update --init --recursive --force --jobs "$JOBS"
fi
"$ESP_ROOT/esp-idf/install.sh" esp32,esp32s3,esp32c6
echo "ESP-IDF $IDF_VERSION ready. Load with: . env.sh"
