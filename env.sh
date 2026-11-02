# Source from the repository root before building firmware. The nix tools are out-links under $ESP_DEV (default $HOME/opt/esp-dev); the
# ESP-IDF and the toolchains are shared under $HOME/opt/esp-idf-6.0 (XIP_ESP_ROOT); build output stays under target/.

if [ -n "${BASH_SOURCE:-}" ]; then
    _em_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
else
    _em_dir="$(pwd)"
fi
export XIP_REPO="${XIP_REPO:-$_em_dir}"
_em_target="$XIP_REPO/target"
mkdir -p "$_em_target" 2>/dev/null || true

export XIP_IDF_VERSION="v6.0.2"
# ESP-IDF and its toolchains (scripts/esp-deps.sh): one installation shared by every workspace, $HOME/opt/esp-idf-6.0, like $ESP_DEV below.
# XIP_ESP_ROOT selects another; an install under this checkout's target/ (older layout) is used when there is no shared one.
if [ -z "${XIP_ESP_ROOT:-}" ]; then
    if [ -d "$HOME/opt/esp-idf-6.0" ] || [ ! -d "$_em_target/esp-idf-6.0" ]; then
        XIP_ESP_ROOT="$HOME/opt/esp-idf-6.0"
    else
        XIP_ESP_ROOT="$_em_target/esp-idf-6.0"
    fi
fi
export XIP_ESP_ROOT
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export TMPDIR="${TMPDIR:-/tmp}"

# Host tools (cmake, ninja, cbindgen, mesh-api-gen, ...): nix is used as a package manager here, not as a shell. scripts/esp-deps.sh builds
# the flake outputs into out-links: `nix build .#esp-dev --out-link $ESP_DEV` (default $HOME/opt/esp-dev) and, for dmesh-cli, lmesh, ...,
# `$ESP_DEV-dmesh`; their bin/ go on PATH. The Rust/wasm tools are the separate `wasm-dev` (modules/WASM.md). A project under active
# development can be put in front with XIP_DEV_BIN (colon separated, for example /ws/dmesh/target/debug), so its own build output wins.
# Older checkouts kept the out-links under target/nix/*: still used when present. A fresh clone with a shared ESP root can reuse the
# profile that sits next to it (<root>/../nix/esp32-profile or <root>/../nix/profile).
export ESP_DEV="${ESP_DEV:-$HOME/opt/esp-dev}"
_em_path=""
for _em_d in "$ESP_DEV/bin" "$ESP_DEV-dmesh/bin" "$_em_target"/nix/*/bin; do
    [ -d "$_em_d" ] && _em_path="$_em_path:$_em_d"
done
if [ -z "$_em_path" ]; then
    for _em_d in "$XIP_ESP_ROOT/../nix/esp32-profile/bin" "$XIP_ESP_ROOT/../nix/profile/bin"; do
        [ -d "$_em_d" ] && { _em_path=":$(cd "$_em_d" && pwd)"; break; }
    done
fi
_em_cli_bin=""
[ ! -x "$_em_target/local/dmesh-cli/bin/dmesh-cli" ] || _em_cli_bin="$_em_target/local/dmesh-cli/bin:"
# The flake's tools (its python3 among them) come before the platform's /usr/bin: the scripts need the python the flake pins, not the distribution's.
export PATH="${XIP_DEV_BIN:+$XIP_DEV_BIN:}${_em_cli_bin}${_em_path#:}${_em_path:+:}/usr/bin:/bin:$PATH"
unset _em_cli_bin
unset _em_path _em_d
# Service catalogs (tools.json per service) for `mesh` and `dmesh-cli`: the dmesh build installs them (flake.nix, dmesh-cli package).
if [ -z "${MESH_SCHEMA_DIR:-}" ] || [ "$MESH_SCHEMA_DIR" = "$_em_target/nix/dmesh/etc/schemas" ] || [ "$MESH_SCHEMA_DIR" = "$ESP_DEV-dmesh/etc/schemas" ]; then
    if [ -d "$_em_target/local/dmesh-cli/etc/schemas" ]; then
        export MESH_SCHEMA_DIR="$_em_target/local/dmesh-cli/etc/schemas"
    elif [ -d "$ESP_DEV-dmesh/etc/schemas" ]; then
        export MESH_SCHEMA_DIR="$ESP_DEV-dmesh/etc/schemas"
    elif [ -d "$_em_target/nix/dmesh/etc/schemas" ]; then
        export MESH_SCHEMA_DIR="$_em_target/nix/dmesh/etc/schemas"
    fi
fi

# ESP-IDF: installed by scripts/esp-deps.sh. Skipped when a container or an
# outer shell already provides idf.py.
if [ -z "${IDF_PATH:-}" ] && [ -f "$XIP_ESP_ROOT/esp-idf/export.sh" ]; then
    export IDF_PATH="$XIP_ESP_ROOT/esp-idf"
    export IDF_TOOLS_PATH="$XIP_ESP_ROOT/espressif"
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

# Sourcing again must keep SDK Python ahead of the host profile/system Python.
if [ -n "${IDF_PYTHON_ENV_PATH:-}" ] && [ -x "$IDF_PYTHON_ENV_PATH/bin/python" ]; then
    export PATH="$IDF_PYTHON_ENV_PATH/bin:$PATH"
fi

unset _em_venv _em_dir _em_target
export XIP_ENV_LOADED=1

# Rust modules (tools/mkmod.py --cargo) need the esp toolchain (nightly + rust-src, Xtensa target): the one that
# comes with the shared ESP-IDF install, if it has it.
if [ -d "$XIP_ESP_ROOT/rustup/toolchains/esp/bin" ]; then
    export XIP_RUST_ESP_BIN="$XIP_ESP_ROOT/rustup/toolchains/esp/bin"
fi
