# Source from the repository root before building firmware. All mutable build
# state (ESP-IDF, toolchains, Nix profile) stays under target/.

if [ -n "${BASH_SOURCE:-}" ]; then
    _em_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
else
    _em_dir="$(pwd)"
fi
export EM_REPO="${EM_REPO:-$_em_dir}"
_em_target="$EM_REPO/target"
mkdir -p "$_em_target" 2>/dev/null || true

export EM_IDF_VERSION="v6.0.2"
# Reuse an existing DMesh checkout's ESP-IDF (target/esp32-6.0) instead of
# downloading another copy. Override with EM_ESP_ROOT, or EM_DMESH_DIR=/path
# (set EM_DMESH_DIR=none to force a private install under target/).
if [ -z "${EM_ESP_ROOT:-}" ]; then
    for _em_cand in "${EM_DMESH_DIR:-}" /ws/dmesh "$EM_REPO/../dmesh"; do
        [ -n "$_em_cand" ] && [ "$_em_cand" != none ] || continue
        _em_root="$_em_cand/target/esp32-6.0"
        if [ -f "$_em_root/esp-idf/export.sh" ] && [ -d "$_em_root/espressif/python_env" ]; then
            EM_ESP_ROOT="$(cd "$_em_root" && pwd)"
            EM_DMESH_REPO="$(cd "$_em_cand" && pwd)"
            break
        fi
    done
fi
export EM_ESP_ROOT="${EM_ESP_ROOT:-$_em_target/esp-idf-6.0}"
export NIX_PROFILE="${EM_NIX_PROFILE:-$_em_target/nix/profile}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export TMPDIR="${TMPDIR:-/tmp}"

# Host tools (cmake, ninja, git): own profile, else DMesh's ESP32 profile.
if [ -n "${EM_DMESH_REPO:-}" ] && [ -d "$EM_DMESH_REPO/target/nix/esp32-profile/bin" ]; then
    export PATH="$EM_DMESH_REPO/target/nix/esp32-profile/bin:$PATH"
fi
if [ ! -d "$NIX_PROFILE/bin" ] && [ -n "${EM_DMESH_REPO:-}" ] && [ -d "$EM_DMESH_REPO/target/nix/profile/bin" ]; then
    NIX_PROFILE="$EM_DMESH_REPO/target/nix/profile"
fi
if [ -d "$NIX_PROFILE/bin" ]; then
    export PATH="/usr/bin:/bin:$NIX_PROFILE/bin:$PATH"
fi

# ESP-IDF: installed by scripts/esp-deps.sh. Skipped when a container or an
# outer shell already provides idf.py.
if [ -z "${IDF_PATH:-}" ] && [ -f "$EM_ESP_ROOT/esp-idf/export.sh" ]; then
    export IDF_PATH="$EM_ESP_ROOT/esp-idf"
    export IDF_TOOLS_PATH="$EM_ESP_ROOT/espressif"
    # Put the install's own Python first: export.sh looks up the venv by the
    # running python3 version (the host python may be a different minor).
    for _em_venv in "$IDF_TOOLS_PATH"/python_env/idf*_env; do
        if [ -x "$_em_venv/bin/python" ]; then
            export IDF_PYTHON_ENV_PATH="$_em_venv"
            export PATH="$_em_venv/bin:$PATH"
        fi
    done
    # export.sh prints a banner; this fragment must stay silent.
    . "$IDF_PATH/export.sh" >/dev/null 2>&1
fi

unset _em_venv _em_dir _em_target _em_cand _em_root
export EM_ENV_LOADED=1

# Rust modules (tools/mkmod.py --cargo) need the esp toolchain (nightly + rust-src, Xtensa target): the one that
# comes with the shared ESP-IDF install, if it has it.
if [ -d "$EM_ESP_ROOT/rustup/toolchains/esp/bin" ]; then
    export EM_RUST_ESP_BIN="$EM_ESP_ROOT/rustup/toolchains/esp/bin"
fi

# esp-wifi-loader owns the scripting: sign.py, sign-bundle.py, upload.py, provision.py, reset.py (and Dockerfile.sign).
# This repository carries no copies. A sibling checkout, or ESP_WIFI_LOADER_DIR.
if [ -z "${ESP_WIFI_LOADER_DIR:-}" ] && [ -d "$EM_REPO/../esp-wifi-loader/tools" ]; then
    export ESP_WIFI_LOADER_DIR="$(cd "$EM_REPO/../esp-wifi-loader" && pwd)"
fi
if [ -n "${ESP_WIFI_LOADER_DIR:-}" ]; then
    case ":$PATH:" in *":$ESP_WIFI_LOADER_DIR/tools:"*) ;; *) export PATH="$ESP_WIFI_LOADER_DIR/tools:$PATH";; esac
fi
