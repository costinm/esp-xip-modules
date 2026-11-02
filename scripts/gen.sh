#!/usr/bin/env bash
# Generate everything that is generated in this project, in one place:  . ./env.sh && scripts/gen.sh
# Whether to commit what it changes (the C header, the manifests inside module images) is the decision to update the runtime or not;
# scripts/test-host.sh is the cross-version test. The tools (cbindgen, cargo, python3) come from the environment (flake.nix);
# mesh-api-gen comes from ssh-mesh (flake, or a checkout in front of it with XIP_DEV_BIN).
#
# From the Rust declarations of crates/xip-abi (the source of truth of the C ABI):
#   include/xip.h                         the C header (cbindgen)
# From api/*.API.md and modules/*/API.md (mesh-api-gen) and the layout of the types (rustdoc's JSON of xip-abi):
#   api/generated/<name>.api.json         the full-fidelity form of each API (docs, enums, the C layout of types and functions)
#   modules/<name>/API.manifest           the compact catalog a module image carries (tools/mkmod.py packs it)
#   modules/mod_con/src/tables.rs         the console's native component layouts and functions
# From modules/*/API.md and the C types in xip-abi (tools/gen_vtables.py), before the header: the vtable of a hosted component and its glue
#   crates/xip-abi/src/ble_vtable.rs      modules/mod_ble/src/api_gen.rs
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

# mesh-api-gen comes from the ssh-mesh flake (the `deps` profile puts it on PATH); the function-table passes below need a version with
# --vtable-conf (a checkout of ssh-mesh: XIP_DEV_BIN=/ws/ssh-mesh/target/debug . ./env.sh).
gen=$(command -v mesh-api-gen || true)
[ -n "$gen" ] || { echo "mesh-api-gen not found: run scripts/esp-deps.sh (nix build .#esp-dev --out-link \$ESP_DEV) and . ./env.sh" >&2; exit 1; }
"$gen" --help | grep -q -- --vtable-conf || { echo "$gen is too old (no --vtable-conf): build ssh-mesh's crates/mesh-api-gen and put it first with XIP_DEV_BIN" >&2; exit 1; }
python3 -c 'import sys; sys.exit(sys.version_info < (3, 12))' || { echo "python3 $(python3 --version) is too old: use the one of the flake (scripts/esp-deps.sh, then . ./env.sh)" >&2; exit 1; }

# the table of each component a module hosts, and the module's glue for it, from the module's API.md and the C types it names
#   crates/xip-abi/src/ble_vtable.rs, modules/mod_ble/src/api_gen.rs   (tools/gen_vtables.py)
python3 tools/gen_vtables.py --api modules/mod_ble/API.md --types crates/xip-abi/src/ble.rs \
    --abi-out crates/xip-abi/src/ble_vtable.rs --module-out modules/mod_ble/src/api_gen.rs

python3 tools/gen_vtables.py --api modules/mod_nan/API.md --types crates/xip-abi/src/nan.rs --local-only \
    --abi-out crates/xip-abi/src/nan_vtable.rs --module-out modules/mod_nan/src/api_gen.rs

python3 tools/gen_vtables.py --api modules/mod_lora/API.md --types crates/xip-abi/src/lib.rs --local-only \
    --abi-out crates/xip-abi/src/lora_vtable.rs --module-out modules/mod_lora/src/api_gen.rs

python3 tools/gen_vtables.py --api modules/mod_system/API.md --types crates/xip-abi/src/system.rs \
    --abi-out crates/xip-abi/src/system_vtable.rs --module-out modules/mod_system/src/api_gen.rs

# Power keeps its native runtime table; NAN owns its selected generated wire methods.
python3 tools/gen_vtables.py --api modules/mod_nan/API.md --types crates/xip-abi/src/power.rs \
    --dispatch-only --methods powerinfo,powersleep,dump_locks --module-out modules/mod_nan/src/power_gen.rs

# the C header
(cd crates/xip-abi && cbindgen --config cbindgen.toml --crate xip-abi 2>/dev/null) | sed 's/\bxip_\([A-Z]\)/XIP_\1/g' > include/xip.h

# the layout of the xip-abi types (RUSTC_BOOTSTRAP lets the stable compiler write rustdoc's JSON)
(cd crates/xip-abi && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$PWD/../../target/rustdoc" \
    cargo rustdoc --quiet --lib -- -Z unstable-options --output-format json >/dev/null 2>"$tmp/rustdoc.log") || { cat "$tmp/rustdoc.log" >&2; exit 1; }
layout=$PWD/target/rustdoc/doc/xip_abi.json

# the schema of the component tables and the wasm32 stubs, from the table structs in that layout (api/vtables.conf lists the tables)
#   crates/xip-abi/src/schema_data.rs, crates/xip-abi/src/vt_shims.rs
mkdir -p api/generated
"$gen" --vtable-conf api/vtables.conf --rustdoc "$layout" --pointer-width 4 --out-tables-json api/generated/tables.api.json
"$gen" --tables-json api/generated/tables.api.json --out-vtable-schema crates/xip-abi/src/schema_data.rs --out-vtable-shims crates/xip-abi/src/vt_shims.rs

mkdir -p api/generated
for src in api/*.API.md; do
    "$gen" --api "$src" --rustdoc "$layout" --pointer-width 4 --out-api-json "api/generated/$(basename "$src" .API.md).api.json"
done

# The binary console needs native component layouts, not a text/name catalog.
"$gen" --api api/wifi.API.md --rustdoc "$layout" --pointer-width 4 --out-console-native "$tmp/native.h"
python3 tools/catalog_rs.py "$tmp/native.h" > modules/mod_con/src/tables.rs

for api in modules/*/API.md; do
    "$gen" --api "$api" --out-manifest "${api%.md}.manifest"
done

# the service catalogs for mesh clients (mesh, dmesh-cli): one directory per component or module, as `mesh::catalog` resolves them
# ($MESH_SCHEMA_DIR/<service>/ or <prefix>/etc/schemas); checked in, and installed by the flake's dmesh-cli package.
#   api/generated/schemas/xip-<name>/{tools.json,schema.json}
rm -rf api/generated/schemas
for api in api/*.API.md modules/*/API.md; do
    case "$api" in
        api/*) name=$(basename "$api" .API.md) ;;
        *) name=$(basename "$(dirname "$api")"); name=${name#mod_} ;;
    esac
    mkdir -p "api/generated/schemas/xip-$name"
    "$gen" --api "$api" --out-tools "api/generated/schemas/xip-$name/tools.json" --out-schema "api/generated/schemas/xip-$name/schema.json"
done
