#!/usr/bin/env bash
# Tests that need no board. CI runs the same.
#   scripts/test-host.sh [all|abi|c|rust|python]     (default: all)
# `c` and `python` need gcc; the module build tests also need the ESP toolchains (. ./env.sh).
# It also checks that what scripts/gen.sh writes is what is committed (the cross-version test of the generated files).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
what="${1:-all}"
mkdir -p target
if [[ $what == abi || $what == all ]]; then
    python3 tests/test_dependencies.py
    # regenerate and compare: a stale generated file is reported (and left regenerated). gen.sh needs cbindgen and mesh-api-gen
    # (on PATH, both come from the flake); without them the check is skipped, the generated files are committed.
    if command -v cbindgen >/dev/null && command -v mesh-api-gen >/dev/null; then
        sums() { sha256sum include/xip.h api/generated/* modules/*/API.manifest modules/mod_con/src/tables.rs; }
        before=$(sums); scripts/gen.sh
        [[ "$before" == "$(sums)" ]] || { echo "scripts/gen.sh changed generated files: commit them (a new runtime) or revert" >&2; exit 1; }
    else
        echo "SKIPPED: generated files not checked (no cbindgen or mesh-api-gen)"
    fi
fi
if [[ $what == c || $what == all ]]; then
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -o target/test_info_cache tests/host/test_info_cache.c -lpthread
    target/test_info_cache
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -o target/test_flash_hal tests/host/test_flash_hal.c components/xip_runtime/src/xip_pool.c
    target/test_flash_hal
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -o target/test_socket_hal tests/host/test_socket_hal.c components/xip_runtime/src/xip_pool.c -lpthread
    target/test_socket_hal
    # the checks a device makes before it maps or installs a module
    gcc -Wall -Wextra -fsanitize=address,undefined -g -I include -I components/xip_runtime/include -I components/xip_loader/include -I tests/host \
        -o target/test_check tests/host/test_check.c components/xip_loader/src/xip_check.c
    target/test_check
    # the packet pool
    gcc -Wall -Wextra -fsanitize=address,undefined -g -I components/xip_runtime/include -o target/test_pool tests/host/test_pool.c \
        components/xip_runtime/src/xip_pool.c -lpthread
    target/test_pool
    # the ESP-NOW action frame: the C codec against the vectors generated from DMesh's code (the Rust twin checks the same file)
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -o target/test_espnow_frame tests/host/test_espnow_frame.c
    target/test_espnow_frame tests/data/espnow-frames.txt
fi
if [[ $what == rust || $what == all ]]; then
    # the crates and the Rust modules: logic on a PC (the images are built by scripts/build.sh with the esp toolchain)
    for crate in modules/mod_lora modules/mod_hw modules/mod_flash; do
        cargo test --quiet --manifest-path "$crate/Cargo.toml"
    done
    # the host-runtime runs the modules (built for this machine) against the host table: virtual time, fixtures
    scripts/build-host-modules.sh >/dev/null
    # crates/: one workspace, one Cargo.lock (xip-abi, both runtimes, dmesh-wifi-profile, dmesh-cli)
    cargo test --quiet --manifest-path crates/Cargo.toml --workspace
fi
if [[ $what == python || $what == all ]]; then
    python3 tests/test_tools.py      # tools, ABI generator, module build (needs the toolchains for that part)
    python3 tests/test_log_history_lock.py   # the firmware's log lock (fw/transport), compiled on the PC with rustc
fi
