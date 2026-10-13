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
# An external ESP-IDF installation may be selected explicitly with EM_ESP_ROOT.
export EM_ESP_ROOT="${EM_ESP_ROOT:-$_em_target/esp-idf-6.0}"
export NIX_PROFILE="${EM_NIX_PROFILE:-$_em_target/nix/profile}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export TMPDIR="${TMPDIR:-/tmp}"

# Host tools (cmake, ninja, git): this checkout's profile.
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

unset _em_venv _em_dir _em_target
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
