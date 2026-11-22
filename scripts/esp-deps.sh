#!/usr/bin/env bash
# Install ESP-IDF v6.0.2 and the esp32/esp32s3/esp32c6 toolchains from the flake
# (`esp-idf` -> $XIP_ESP_ROOT, default $HOME/opt/esp-idf) and host tools (`esp-dev` -> $ESP_DEV,
# default $HOME/opt/esp-dev; env.sh puts its bin/ on PATH).
#   scripts/esp-deps.sh host                      only the host tools
#   scripts/esp-deps.sh dmesh                     also the DMesh and ssh-mesh tools
#   scripts/esp-deps.sh --bootstrap-from-network  imperative upstream git clone + espup fallback
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$REPO_ROOT/env.sh"
ESP_ROOT="$XIP_ESP_ROOT"
DEPS="$ESP_DEV"
IDF_VERSION="v6.0.2"
JOBS="${ESP_IDF_GIT_JOBS:-8}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
mkdir -p "$(dirname "$DEPS")" "$(dirname "$ESP_ROOT")"

NIX_BIN="$(command -v nix || true)"
if [[ -z "$NIX_BIN" && -x /nix/var/nix/profiles/default/bin/nix ]]; then
    NIX_BIN=/nix/var/nix/profiles/default/bin/nix
fi

if [[ ${1:-} != --bootstrap-from-network ]] && [[ -n "$NIX_BIN" ]]; then
    # ssh-mesh and DMesh change together with this repository and are hard to coordinate through commits: build from the local checkouts
    # (DMESH_DIR, SSH_MESH_DIR; defaults /ws/dmesh and /ws/rust/ssh-mesh when present) instead of the locked revisions. XIP_NIX_LOCKED=1 uses the lock.
    over=()
    if [[ -z ${XIP_NIX_LOCKED:-} ]]; then
        [[ -d ${DMESH_DIR:=/ws/dmesh} ]] && over+=(--override-input dmesh "git+file://$(readlink -f "$DMESH_DIR")")
        if [[ -z ${SSH_MESH_DIR:-} ]]; then
            [[ -d /ws/rust/ssh-mesh ]] && SSH_MESH_DIR=/ws/rust/ssh-mesh || SSH_MESH_DIR=/ws/ssh-mesh
        fi
        [[ -d ${SSH_MESH_DIR:-} ]] && over+=(--override-input ssh-mesh "git+file://$(readlink -f "$SSH_MESH_DIR")")
    fi
    # the flake of this checkout (tracked files; a `path:` reference would copy the whole target/ tree into the store)
    "$NIX_BIN" build "${over[@]}" --out-link "$DEPS" "$REPO_ROOT#esp-dev"
    if [[ ${1:-} == dmesh ]]; then
        "$NIX_BIN" build "${over[@]}" --out-link "$ESP_DEV-dmesh" "$REPO_ROOT#dmesh"
        echo "DMesh tools ready: $ESP_DEV-dmesh/bin (dmesh-cli, lmesh, mesh, ...)"
        exit 0
    fi
    if [[ ${1:-} == host ]]; then
        echo "Host tools ready: $DEPS"
        exit 0
    fi
    if [[ -L "$ESP_ROOT" ]] && [[ "$(readlink "$ESP_ROOT")" != /nix/store/* ]]; then
        rm -f "$ESP_ROOT"
    fi
    "$NIX_BIN" build "${over[@]}" --out-link "$ESP_ROOT" "$REPO_ROOT#esp-idf"
    echo "ESP-IDF $IDF_VERSION ready at $ESP_ROOT. Load with: . env.sh"
    exit 0
fi

if [[ ${1:-} != --bootstrap-from-network ]]; then
    if [[ ! -x "$DEPS/bin/cmake" ]]; then
        echo "nix is unavailable and $DEPS has no host tools" >&2
        exit 127
    fi
    if [[ ${1:-} == host ]]; then
        echo "Host tools ready: $DEPS"
        exit 0
    fi
    echo "nix is unavailable; re-run with --bootstrap-from-network to install ESP-IDF imperatively into $ESP_ROOT" >&2
    exit 1
fi

mkdir -p "$ESP_ROOT" "$ESP_ROOT/espressif"
export IDF_TOOLS_PATH="$ESP_ROOT/espressif"
export PATH="$DEPS/bin:$PATH"
if [[ ! -x "$DEPS/bin/espup" || ! -x "$DEPS/bin/ldproxy" ]]; then
    echo "incomplete host tools: $DEPS needs espup and ldproxy" >&2
    exit 1
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
