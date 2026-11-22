#!/usr/bin/env bash
# Build and stage this checkout's CLI and matching catalogs on the local PATH.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. ./env.sh
profile=debug
cargo_flags=()
case ${1:-} in
    "") ;;
    --release) profile=release; cargo_flags+=(--release) ;;
    --help) echo "scripts/build-cli.sh [--release]"; exit 0 ;;
    *) echo "scripts/build-cli.sh [--release]" >&2; exit 2 ;;
esac
if ! command -v mesh-api-gen >/dev/null || ! mesh-api-gen --help 2>&1 | grep -q -- --out-xip-vtable; then
    for _cand in /ws/rust/ssh-mesh/target/x86_64-unknown-linux-musl/release /ws/rust/ssh-mesh/target/x86_64-unknown-linux-musl/debug; do
        if [ -x "$_cand/mesh-api-gen" ] && "$_cand/mesh-api-gen" --help 2>&1 | grep -q -- --out-xip-vtable; then
            export PATH="$_cand:$PATH"
            break
        fi
    done
fi
if command -v cbindgen >/dev/null && command -v mesh-api-gen >/dev/null; then
    if [ -f "$ROOT/api/generated/host.api.json" ]; then
        XIP_RUNTIME_VERSION="${XIP_RUNTIME_VERSION:-1}" scripts/gen.sh --schemas-only
    else
        XIP_RUNTIME_VERSION="${XIP_RUNTIME_VERSION:-1}" scripts/gen.sh
    fi
fi
cargo build --locked --manifest-path crates/dmesh-cli/Cargo.toml "${cargo_flags[@]}"
cargo_target=$(cargo metadata --no-deps --format-version 1 --manifest-path crates/Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
package="$ROOT/target/local/dmesh-cli"
mkdir -p "$package/bin" "$package/etc/schemas/lmesh" "$ROOT/target/nix"
install -m755 "$cargo_target/$profile/dmesh-cli" "$package/bin/.dmesh-cli-real.new"
mv "$package/bin/.dmesh-cli-real.new" "$package/bin/.dmesh-cli-real"
rm -rf "$package/etc/schemas"
mkdir -p "$package/etc/schemas/lmesh"
if [ -d target/schemas ]; then
    cp -r target/schemas/. "$package/etc/schemas/"
fi
if [ -d api/generated/schemas ]; then
    cp -r api/generated/schemas/. "$package/etc/schemas/"
fi
cp "$ROOT/crates/dmesh-cli/schemas/lmesh/tools.json" "$package/etc/schemas/lmesh/tools.json"
cat > "$package/bin/dmesh-cli.new" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
package=$(cd "$(dirname "$0")/.." && pwd -P)
repo=$(cd "$package/../../.." && pwd -P)
# Replace only this checkout's old env.sh default, preserving explicit catalogs.
case ${MESH_SCHEMA_DIR:-} in
    ""|"$repo/target/nix/dmesh/etc/schemas") export MESH_SCHEMA_DIR="$package/etc/schemas" ;;
esac
exec "$package/bin/.dmesh-cli-real" "$@"
WRAPPER
chmod +x "$package/bin/dmesh-cli.new"
mv "$package/bin/dmesh-cli.new" "$package/bin/dmesh-cli"
link="$ROOT/target/nix/dmesh-cli"
if [[ -e $link && ! -L $link ]]; then
    echo "Refusing to replace directory $link" >&2
    exit 1
fi
if [[ -L $link && ! -f $ROOT/target/local/dmesh-cli.previous-link ]]; then
    readlink "$link" > "$ROOT/target/local/dmesh-cli.previous-link"
fi
ln -sfn "$package" "$link"
printf 'CLI ready: %s\n' "$link/bin/dmesh-cli"
