#!/usr/bin/env bash
# Generate everything that is generated in this project, in one place, consistently. Needs nothing but a checkout and `. ./env.sh` (the script sources it):
# the tools it uses it finds or builds itself.
#   scripts/gen.sh           write the generated files
#   scripts/gen.sh --check   fail if any of them is stale (CI, scripts/test-host.sh)
#
# From the Rust declarations of crates/xip-abi (the source of truth of the C ABI):
#   include/xip.h                         the C header (cbindgen)
# From api/*.API.md and modules/*/API.md (the CBOR / JSON / text view of the APIs; mesh-api-gen from the ssh-mesh checkout, built into target/ on first use) and the
# layout of the types that rustdoc's JSON of xip-abi gives:
#   api/generated/<name>.api.json         the one full-fidelity form of each API (docs, enums, the C layout of types and functions)
#   modules/<name>/API.manifest           the compact catalog a module image carries (tools/mkmod.py packs it)
#   modules/mod_console/{catalog,native}.h   the console's tables: the methods by name, and where the native functions of the host components are
#   modules/mod_con/src/tables.rs         the same tables as Rust constants (tools/catalog_rs.py, until mesh-api-gen writes Rust for them)
# Builds use the checked-in output: nothing here is a build dependency.
set -euo pipefail
cd "$(dirname "$0")/.."
. ./env.sh >/dev/null 2>&1 || true
mode=write; [ "${1:-}" = "--check" ] && mode=check
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
status=0

# ---- tools: found on the PATH, or built here, never configured ----
if command -v cbindgen >/dev/null 2>&1; then cbindgen=(cbindgen)
elif command -v nix >/dev/null 2>&1; then cbindgen=(nix run nixpkgs#rust-cbindgen --)
else echo "gen: cbindgen is needed (cargo install cbindgen, or nix)" >&2; exit 1; fi

if command -v mesh-api-gen >/dev/null 2>&1; then gen=$(command -v mesh-api-gen)
else
    src=../ssh-mesh/crates/mesh-api-gen/Cargo.toml
    [ -f "$src" ] || { echo "gen: mesh-api-gen needs the ssh-mesh checkout next to this one (../ssh-mesh), or on the PATH" >&2; exit 1; }
    CARGO_TARGET_DIR="$PWD/target/mesh-api-gen" cargo build --quiet --manifest-path "$src" >&2
    gen=$PWD/target/mesh-api-gen/debug/mesh-api-gen
fi

# keep DEST equal to SRC (write), or say that it is not (check)
commit() {
    local src=$1 dest=$2
    if [ $mode = check ]; then
        cmp -s "$src" "$dest" || { echo "gen: $dest is stale; run scripts/gen.sh" >&2; status=1; }
    else
        mkdir -p "$(dirname "$dest")"
        cmp -s "$src" "$dest" 2>/dev/null || { cp "$src" "$dest"; echo "wrote $dest"; }
    fi
}

# ---- include/xip.h ----
(cd crates/xip-abi && "${cbindgen[@]}" --config cbindgen.toml --crate xip-abi 2>/dev/null) | sed 's/\bxip_\([A-Z]\)/XIP_\1/g' > "$tmp/xip.h"
commit "$tmp/xip.h" include/xip.h

# ---- the layout of the xip-abi types (rustdoc's JSON; RUSTC_BOOTSTRAP lets the stable compiler write it) ----
(cd crates/xip-abi && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$PWD/../../target/rustdoc" \
    cargo rustdoc --quiet --lib -- -Z unstable-options --output-format json >/dev/null 2>"$tmp/rustdoc.log") \
    || { cat "$tmp/rustdoc.log" >&2; exit 1; }
layout=$PWD/target/rustdoc/doc/xip_abi.json

# ---- api/generated/<name>.api.json ----
for src in api/*.API.md; do
    name=$(basename "$src" .API.md)
    "$gen" --api "$src" --rustdoc "$layout" --pointer-width 4 --out-api-json "$tmp/$name.api.json"
    commit "$tmp/$name.api.json" "api/generated/$name.api.json"
done

# ---- the console's tables: every API the console can call ----
# (the C and Rust twins of a module share a component: mod_con is mod_console's, mod_rtest is mod_test's - the catalog takes one of each; the host's own calls, api/host.API.md, are not for the console)
apis=(); for m in modules/*/API.md api/*.API.md; do case $m in modules/mod_con/*|modules/mod_rtest/*|api/host.API.md) ;; *) apis+=(--api "$m");; esac; done
"$gen" "${apis[@]}" --out-console-catalog "$tmp/catalog.h"
commit "$tmp/catalog.h" modules/mod_console/catalog.h
"$gen" --api api/wifi.API.md --api api/power.API.md --rustdoc "$layout" --pointer-width 4 --out-console-native "$tmp/native.h"
commit "$tmp/native.h" modules/mod_console/native.h
python3 tools/catalog_rs.py "$tmp/catalog.h" "$tmp/native.h" | sed "s|from catalog.h, native.h|from catalog.h, native.h|" > "$tmp/tables.rs"
commit "$tmp/tables.rs" modules/mod_con/src/tables.rs

# ---- modules/<name>/API.manifest ----
for api in modules/*/API.md; do
    "$gen" --api "$api" --out-manifest "$tmp/module.manifest"
    commit "$tmp/module.manifest" "${api%.md}.manifest"
done
[ $mode = check ] && [ $status = 0 ] && echo "gen: up to date"
exit $status
