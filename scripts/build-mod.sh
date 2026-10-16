#!/usr/bin/env bash
# Build one C or Rust module directory with the XIP-owned tools.
set -euo pipefail
ROOT="${XIP_TOOL_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
if [[ ${1:-} == --help || $# == 0 ]]; then
    echo 'Usage: build-mod.sh DIRECTORY [--chip esp32c6|esp32|esp32s3] [--slots HOST_SOURCE] [--slot NAME] [--out DIRECTORY]'
    exit 0
fi
# In a checkout env.sh owns the SDK/profile. An installed builder uses the
# caller environment (source that project's env.sh before invoking it).
if ! command -v idf.py >/dev/null 2>&1 && [[ -f "$ROOT/env.sh" ]]; then
    . "$ROOT/env.sh"
fi
exec python3 "$ROOT/tools/build_mod.py" "$@"
