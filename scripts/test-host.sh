#!/usr/bin/env bash
# Tests that need no board. CI uses only generated, ci-runtime and cli (no root tests/ dependency).
#   scripts/test-host.sh [all|abi|generated|c|rust|runtime|ci-runtime|cli|python]     (default: all)
# `c` and `python` need gcc; the module build tests also need the ESP toolchains (. ./env.sh).
# The abi/all group compares generated files before/after regeneration in the current tree (not Git HEAD).
# Most failures stop immediately; CLI test failures are collected so all can still run Python checks.
# Any collected failure still makes the script exit nonzero.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
what="${1:-all}"
case "$what" in
    all|abi|generated|c|rust|runtime|ci-runtime|cli|python) ;;
    *) echo "usage: scripts/test-host.sh [all|abi|generated|c|rust|runtime|ci-runtime|cli|python]" >&2; exit 2 ;;
esac
KNOWN_FAILURES=0
mkdir -p target
if [[ $what == abi || $what == all ]]; then
    python3 tests/test_dependencies.py
fi
if [[ $what == generated || $what == abi || $what == all ]]; then
    # regenerate and compare: a stale generated file is reported (and left regenerated). gen.sh needs cbindgen and mesh-api-gen
    # (on PATH, both come from the flake); without them the check is skipped, the generated files are committed.
    if command -v cbindgen >/dev/null && command -v mesh-api-gen >/dev/null; then
        sums() { find api/generated -type f | LC_ALL=C sort | xargs sha256sum; sha256sum include/xip.h modules/*/API.manifest modules/mod_con/src/tables.rs; }
        before=$(sums); scripts/gen.sh
        [[ "$before" == "$(sums)" ]] || { echo "scripts/gen.sh changed generated files: commit them (a new runtime) or revert" >&2; exit 1; }
    elif [[ $what == generated ]]; then
        echo "ERROR: generated checks require cbindgen and mesh-api-gen" >&2
        exit 1
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
    # the PPP codec of the serial line, on a fake pool
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include -I tests/host/fake_idf -I tests/host \
        -o target/test_ppp tests/host/test_ppp.c components/xip_runtime/src/xip_ppp.c
    target/test_ppp
    # NOW framing is private to the bearer; its maintained Rust native tests run in ci-runtime.
fi
if [[ $what == rust || $what == runtime || $what == ci-runtime || $what == all ]]; then
    # the crates and the Rust modules: logic on a PC (the images are built by scripts/build.sh with the esp toolchain)
    module_crates=(modules/mod_lora modules/mod_hw)
    # Legacy mod_flash compatibility tests read local root tests/data fixtures.
    # Keep them in local runtime/rust/all runs until rewritten as self-contained Rust tests.
    [[ $what == ci-runtime ]] || module_crates+=(modules/mod_flash)
    for crate in "${module_crates[@]}"; do
        cargo test --quiet --manifest-path "$crate/Cargo.toml"
    done
    # the host-runtime runs the modules (built for this machine) against the host table: virtual time, fixtures
    scripts/build-host-modules.sh >/dev/null
    # crates/: one workspace, one Cargo.lock (xip-abi, both runtimes, dmesh-wifi-profile, dmesh-cli)
    cargo test --quiet --locked --manifest-path crates/Cargo.toml --workspace --exclude dmesh-cli
fi
if [[ $what == rust || $what == cli || $what == all ]]; then
    # dmesh-cli separately: its schema tests are known to fail while the pinned dmesh-server git dep
    # (crates/dmesh-cli/Cargo.toml) predates the newer `transport.set` fields. Report, keep going so the
    # remaining steps still run, and fail at the end. Bumping the pin clears this.
    if ! cargo test --quiet --locked --manifest-path crates/Cargo.toml -p dmesh-cli --lib --bins; then
        echo "FAILED (known): dmesh-cli schema tests vs the pinned dmesh-server dep; bump the pin to clear" >&2
        KNOWN_FAILURES=1
    fi
fi
if [[ $what == python || $what == all ]]; then
    python3 tests/test_tools.py      # tools, ABI generator, module build (needs the toolchains for that part)
    python3 tests/test_log_history_lock.py   # the firmware's log lock (fw/transport), compiled on the PC with rustc
fi
if [[ $KNOWN_FAILURES != 0 ]]; then
    echo "test-host: requested groups finished; dmesh-cli failures above remain (pinned dmesh-server dep)" >&2
    exit 1
fi
