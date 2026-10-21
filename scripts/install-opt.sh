#!/usr/bin/env bash
# Install the service catalogs of this repository's components (xip-wifi, xip-power, xip-con, ...) from the nix build of the DMesh tools
# into $HOME/opt/<service>/etc/schemas, where `mesh` and `dmesh-cli` look for them without MESH_SCHEMA_DIR. They are symlinks into the
# nix store: rerun after `scripts/esp-deps.sh dmesh` to update them (when things are relatively stable; fine during development too).
#   scripts/install-opt.sh            xip-* services
#   scripts/install-opt.sh --lmesh    also $HOME/opt/lmesh/etc/schemas/tools.json (the lmesh service's state directory: only that file)
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$(readlink -f "$REPO_ROOT/target/nix/dmesh")"
[[ -d $out/etc/schemas ]] || { echo "no $REPO_ROOT/target/nix/dmesh: run scripts/esp-deps.sh dmesh" >&2; exit 1; }
opt="${HOME}/opt"
for dir in "$out"/etc/schemas/xip-*; do
    svc="$(basename "$dir")"
    mkdir -p "$opt/$svc/etc"
    ln -sfn "$dir" "$opt/$svc/etc/schemas"
    echo "$opt/$svc/etc/schemas -> $dir"
done
if [[ ${1:-} == --lmesh ]]; then
    mkdir -p "$opt/lmesh/etc/schemas"
    ln -sfn "$out/etc/schemas/lmesh/tools.json" "$opt/lmesh/etc/schemas/tools.json"
    echo "$opt/lmesh/etc/schemas/tools.json -> $out/etc/schemas/lmesh/tools.json"
fi
