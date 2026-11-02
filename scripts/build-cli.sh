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
cargo build --locked --manifest-path crates/dmesh-cli/Cargo.toml "${cargo_flags[@]}"
cargo_target=$(cargo metadata --no-deps --format-version 1 --manifest-path crates/Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
package="$ROOT/target/local/dmesh-cli"
mkdir -p "$package/bin" "$package/etc/schemas/lmesh" "$ROOT/target/nix"
install -m755 "$cargo_target/$profile/dmesh-cli" "$package/bin/.dmesh-cli-real.new"
mv "$package/bin/.dmesh-cli-real.new" "$package/bin/.dmesh-cli-real"
cp -r api/generated/schemas/. "$package/etc/schemas/"
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
