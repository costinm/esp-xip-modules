#!/usr/bin/env bash
# Generate everything that is generated in this project, in one place:  . ./env.sh && scripts/gen.sh
# Whether to commit what it changes (the C header, the manifests inside module images) is the decision to update the runtime or not;
# scripts/test-host.sh is the cross-version test. The tools (cbindgen, cargo, python3) come from the environment (flake.nix);
# mesh-api-gen comes from ssh-mesh (flake, or a checkout in front of it with XIP_DEV_BIN).
#
# From the Rust declarations of crates/xip-abi (the source of truth of the C ABI):
#   include/xip.h                         the C header (cbindgen)
# From api/*.API.md, modules/*/API.md and crates/*/API.md (mesh-api-gen) and the layout of the types (rustdoc's JSON of xip-abi):
#   api/generated/<name>.api.json         the full-fidelity form of each API (docs, enums, the C layout of types and functions)
#   modules/<name>/API.manifest           the module's name, component and API catalog (tools/mkmod.py packs only keys 1 and 2 into an image)
#   crates/<name>/API.md                  a component crate (sys, hw): its API, no manifest (a module packs its own)
#   crates/sys/src/tables.rs              the sys component's native component layouts and functions
# From modules/*/API.md and the C types in xip-abi (tools/gen_vtables.py), before the header: the vtable of a hosted component and its glue
#   crates/xip-abi/src/ble_vtable.rs      modules/mod_ble/src/api_gen.rs
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

# mesh-api-gen comes from the ssh-mesh flake (the `deps` profile puts it on PATH); the XIP table/catalog passes below need a version with
# --out-xip-vtable / --out-console-native-rs (a checkout of ssh-mesh: XIP_DEV_BIN=/ws/rust/ssh-mesh/target/x86_64-unknown-linux-musl/debug . ./env.sh).
gen=$(command -v mesh-api-gen || true)
[ -n "$gen" ] || { echo "mesh-api-gen not found: run scripts/esp-deps.sh (nix build .#esp-dev --out-link \$ESP_DEV) and . ./env.sh" >&2; exit 1; }
"$gen" --help | grep -q -- --out-xip-vtable || { echo "$gen is too old (no --out-xip-vtable): build ssh-mesh's crates/mesh-api-gen and put it first with XIP_DEV_BIN" >&2; exit 1; }
python3 -c 'import sys; sys.exit(sys.version_info < (3, 12))' || { echo "python3 $(python3 --version) is too old: use the one of the flake (scripts/esp-deps.sh, then . ./env.sh)" >&2; exit 1; }

# the table of each component a module hosts, and the module's glue for it, from the module's API.md and the C types it names
"$gen" --api modules/mod_ble/API.md --xip-types crates/xip-abi/src/ble.rs \
    --out-xip-vtable crates/xip-abi/src/ble_vtable.rs --out-xip-module modules/mod_ble/src/api_gen.rs

"$gen" --api modules/mod_nan/API.md --xip-types crates/xip-abi/src/nan.rs --xip-local-only \
    --out-xip-vtable crates/xip-abi/src/nan_vtable.rs --out-xip-module modules/mod_nan/src/api_gen.rs

"$gen" --api modules/mod_lora/API.md --xip-types crates/xip-abi/src/lib.rs --xip-local-only \
    --out-xip-vtable crates/xip-abi/src/lora_vtable.rs --out-xip-module modules/mod_lora/src/api_gen.rs

# (the device-control methods 5-8, 10-11 are CBOR-only: the table and its adapter have the diagnostics 1-3 alone)
"$gen" --api crates/sys/API.md --xip-types crates/xip-abi/src/sys.rs --xip-table-methods heap,heap_dump,dump_tasks \
    --out-xip-vtable crates/xip-abi/src/sys_vtable.rs --out-xip-module crates/sys/src/api_gen.rs

# Power keeps its native runtime table; NAN owns its selected generated wire methods.
"$gen" --api modules/mod_nan/API.md --xip-types crates/xip-abi/src/power.rs \
    --xip-dispatch-only --xip-methods powerinfo,powersleep,dump_locks --out-xip-module modules/mod_nan/src/power_gen.rs

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

# The sys component's native dispatch needs native component layouts, not a text/name catalog.
"$gen" --api api/wifi.API.md --rustdoc "$layout" --pointer-width 4 --out-console-native-rs crates/sys/src/tables.rs

# A module's own API.md is modules/<name>/API.md. The components a module composes are crates (crates/<name>/API.md, the title
# `# \`name\` API (id)`): they get no manifest, the module packs its own.
for api in modules/*/API.md; do
    head -1 "$api" | grep -qE '^# `[a-z0-9_-]+` API \([0-9]+\)$' || continue
    "$gen" --api "$api" --out-manifest "${api%.md}.manifest"
done

# the service catalogs for mesh clients (mesh, dmesh-cli): one directory per component or module, as `mesh::catalog` resolves them
# ($MESH_SCHEMA_DIR/<service>/ or <prefix>/etc/schemas); checked in, and installed by the flake's dmesh-cli package.
#   api/generated/schemas/xip-<name>/{api.cbor,tools.json,schema.json}
# The scan takes crates/*/API.md (any component crate with that title) as well. Discovering the API.md of dependency crates (a crate that
# arrives from another repository as a Cargo git dependency) through `cargo metadata` is future work: until then such a crate is listed here.
rm -rf api/generated/schemas
for api in api/*.API.md modules/*/API.md crates/*/API.md; do
    [ -f "$api" ] || continue
    head -1 "$api" | grep -qE '^# `[a-z0-9_-]+` API \([0-9]+\)$' || continue
    case "$api" in
        api/*) name=$(basename "$api" .API.md) ;;
        crates/*) name=$(basename "$(dirname "$api")") ;;
        *) name=$(basename "$(dirname "$api")"); name=${name#mod_} ;;
    esac
    mkdir -p "api/generated/schemas/xip-$name"
    "$gen" --api "$api" --rustdoc "$layout" --pointer-width 4 \
        --out-api-cbor "api/generated/schemas/xip-$name/api.cbor" \
        --out-tools "api/generated/schemas/xip-$name/tools.json" --out-schema "api/generated/schemas/xip-$name/schema.json"
done
