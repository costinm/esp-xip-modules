# Source from the repository root before building firmware. All mutable build
# state (ESP-IDF, toolchains, nix out-links) stays under target/.

if [ -n "${BASH_SOURCE:-}" ]; then
    _em_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
else
    _em_dir="$(pwd)"
fi
export XIP_REPO="${XIP_REPO:-$_em_dir}"
_em_target="$XIP_REPO/target"
mkdir -p "$_em_target" 2>/dev/null || true

export XIP_IDF_VERSION="v6.0.2"
# An external ESP-IDF installation may be selected explicitly with XIP_ESP_ROOT.
export XIP_ESP_ROOT="${XIP_ESP_ROOT:-$_em_target/esp-idf-6.0}"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
export TMPDIR="${TMPDIR:-/tmp}"

# Host tools (cmake, ninja, cbindgen, mesh-api-gen, ...): nix is used as a package manager here, not as a shell. scripts/esp-deps.sh builds
# the flake outputs into out-links (`nix build --out-link target/nix/deps`, and target/nix/dmesh for dmesh-cli, lmesh, ...); the bin/ of
# every link under target/nix goes on PATH. A project under active development can be put in front with XIP_DEV_BIN (colon separated, for
# example /ws/dmesh/target/debug), so its own build output wins over the nix one.
# A fresh clone with a shared ESP root can reuse the profile that sits next to it (<root>/../nix/esp32-profile or <root>/../nix/profile).
_em_path=""
for _em_d in "$_em_target"/nix/*/bin; do
    [ -d "$_em_d" ] && _em_path="$_em_path:$_em_d"
done
if [ -z "$_em_path" ]; then
    for _em_d in "$XIP_ESP_ROOT/../nix/esp32-profile/bin" "$XIP_ESP_ROOT/../nix/profile/bin"; do
        [ -d "$_em_d" ] && { _em_path=":$(cd "$_em_d" && pwd)"; break; }
    done
fi
export PATH="${XIP_DEV_BIN:+$XIP_DEV_BIN:}/usr/bin:/bin${_em_path}:$PATH"
unset _em_path _em_d
# Service catalogs (tools.json per service) for `mesh` and `dmesh-cli`: the dmesh build installs them (flake.nix, dmesh-cli package).
if [ -z "${MESH_SCHEMA_DIR:-}" ] && [ -d "$_em_target/nix/dmesh/etc/schemas" ]; then
    export MESH_SCHEMA_DIR="$_em_target/nix/dmesh/etc/schemas"
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
