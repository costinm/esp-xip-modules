#!/usr/bin/env bash
# Replace the rolling GitHub release `latest` with the files in REL (default target/rel; scripts/publish-release.sh).
#   scripts/publish-latest.sh VERSION          (needs gh, authenticated: GH_TOKEN in CI)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REL="${REL:-$ROOT/target/rel}"
version="${1:?version}"
repo="${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
sha="${GITHUB_SHA:-$(git -C "$ROOT" rev-parse HEAD)}"
gh release delete latest --repo "$repo" --cleanup-tag --yes || true
gh release create latest "$REL"/* --repo "$repo" --target "$sha" --title "latest ($version)" \
    --notes "Signed main (<chip>.wlpkg, for esp-wifi-loader) and modules (<chip>-<module>.wlpkg, for the upload port of a running main), version $version, commit $sha. Source for the loader: $repo:latest."
