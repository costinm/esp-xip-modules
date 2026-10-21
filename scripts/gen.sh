#!/usr/bin/env bash
# Generate everything that is generated in this project, in one place:  . ./env.sh && scripts/gen.sh
# Whether to commit what it changes (the C header, the manifests inside module images) is the decision to update the runtime or not;
# scripts/test-host.sh is the cross-version test. The tools (cbindgen, cargo, python3) come from the environment (flake.nix);
# mesh-api-gen comes from the ssh-mesh flake (the `deps` profile of flake.nix puts it on PATH).
#
# From the Rust declarations of crates/xip-abi (the source of truth of the C ABI):
#   include/xip.h                         the C header (cbindgen)
# From api/*.API.md and modules/*/API.md (mesh-api-gen) and the layout of the types (rustdoc's JSON of xip-abi):
#   api/generated/<name>.api.json         the full-fidelity form of each API (docs, enums, the C layout of types and functions)
#   modules/<name>/API.manifest           the compact catalog a module image carries (tools/mkmod.py packs it)
#   modules/mod_con/src/tables.rs         the console's tables: the methods by name, and where the native functions of the host components are
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

gen=$(command -v mesh-api-gen || true)
[ -n "$gen" ] || { echo "mesh-api-gen not found: run scripts/esp-deps.sh (nix build --out-link target/nix/deps) and . ./env.sh" >&2; exit 1; }

# the C header
(cd crates/xip-abi && cbindgen --config cbindgen.toml --crate xip-abi 2>/dev/null) | sed 's/\bxip_\([A-Z]\)/XIP_\1/g' > include/xip.h

# the layout of the xip-abi types (RUSTC_BOOTSTRAP lets the stable compiler write rustdoc's JSON)
(cd crates/xip-abi && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$PWD/../../target/rustdoc" \
    cargo rustdoc --quiet --lib -- -Z unstable-options --output-format json >/dev/null 2>"$tmp/rustdoc.log") || { cat "$tmp/rustdoc.log" >&2; exit 1; }
layout=$PWD/target/rustdoc/doc/xip_abi.json

mkdir -p api/generated
for src in api/*.API.md; do
    "$gen" --api "$src" --rustdoc "$layout" --pointer-width 4 --out-api-json "api/generated/$(basename "$src" .API.md).api.json"
done

# the console: every API it can call (the host's own calls, api/host.API.md, are not for it)
apis=(); for m in modules/*/API.md api/*.API.md; do [ "$m" = api/host.API.md ] || apis+=(--api "$m"); done
"$gen" "${apis[@]}" --out-console-catalog "$tmp/catalog.h"
"$gen" --api api/wifi.API.md --api api/power.API.md --rustdoc "$layout" --pointer-width 4 --out-console-native "$tmp/native.h"
python3 tools/catalog_rs.py "$tmp/catalog.h" "$tmp/native.h" > modules/mod_con/src/tables.rs

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
