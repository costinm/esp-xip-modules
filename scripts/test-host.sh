#!/usr/bin/env bash
# Tests that need no board. CI runs the same.
#   scripts/test-host.sh [all|abi|c|rust|python]     (default: all)
# `c` and `python` need gcc; the module build tests also need the ESP toolchains (. ./env.sh).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
what="${1:-all}"
mkdir -p target
# esp-wifi-loader owns sign.py, wl_format.py, upload.py ... (tests/loader_tools.py finds them)
export ESP_WIFI_LOADER_DIR="${ESP_WIFI_LOADER_DIR:-$ROOT/../esp-wifi-loader}"
if [[ $what == abi || $what == all ]]; then
    python3 tools/gen_abi.py --check
    python3 tools/gen_catalog.py --check
fi
if [[ $what == c || $what == all ]]; then
    # the checks a device makes before it maps or installs a module
    gcc -Wall -Wextra -fsanitize=address,undefined -g -I include -I components/espmod_pool/include -I components/espmod_loader/include -I tests/host \
        -o target/test_check tests/host/test_check.c components/espmod_loader/src/espmod_check.c
    target/test_check
    # the console module (text <-> tagged CBOR) and the CBOR helpers
    gcc -Wall -Wextra -fsanitize=address,undefined -g -DESPMOD_LINKED=1 -I include -I modules/common -I modules/mod_console \
        -I tests/host -I components/espmod_pool/include -o target/test_console tests/host/test_console.c tests/host/sim_host.c components/espmod_pool/src/pool.c modules/mod_console/mod_console.c modules/mod_test/mod_test.c
    target/test_console
    # mod_hello and mod_test, the sources that become flash images, against a simulated host
    gcc -Wall -Wextra -fsanitize=address,undefined -g -I components/espmod_pool/include -o target/test_pool tests/host/test_pool.c \
        components/espmod_pool/src/pool.c -lpthread
    target/test_pool
    gcc -Wall -Wextra -fsanitize=address,undefined -g -DESPMOD_LINKED=1 -I include -I modules/common -I tests/host \
        -I components/espmod_pool/include -o target/test_modules tests/host/test_modules.c tests/host/sim_host.c \
        components/espmod_pool/src/pool.c modules/mod_hello/mod_hello.c \
        modules/mod_test/mod_test.c
    target/test_modules
    # the same tests against the Rust twin (modules/mod_rtest), as a static library for the PC
    RUSTC_BOOTSTRAP=1 cargo rustc --quiet --release --lib --crate-type staticlib --features sim --manifest-path modules/mod_rtest/Cargo.toml
    gcc -Wall -Wextra -fsanitize=address,undefined -g -DTEST_HANDLE=espmod_handle -DNO_HELLO -I include -I modules/common -I tests/host \
        -I components/espmod_pool/include -o target/test_rtest tests/host/test_modules.c tests/host/sim_host.c \
        components/espmod_pool/src/pool.c modules/mod_rtest/target/release/libesp_mod_rtest.a -lpthread -ldl
    target/test_rtest
fi
if [[ $what == rust || $what == all ]]; then
    # the crates and the Rust modules: logic on a PC (the images are built by scripts/build.sh with the esp toolchain)
    for crate in crates/espmod-abi crates/espmod-host modules/mod_lora modules/mod_hw; do
        cargo test --quiet --manifest-path "$crate/Cargo.toml"
    done
fi
if [[ $what == python || $what == all ]]; then
    python3 tests/test_tools.py      # tools, ABI generator, module build (needs the toolchains for that part)
    python3 tests/test_install.py    # the module sink of esp-wifi-loader, compiled for the host
fi
