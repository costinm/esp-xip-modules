#!/usr/bin/env bash
# Tests that need no board. Source env.sh; device tests use tests/e2e/run.py.
#   scripts/test-host.sh [all|abi|generated|c|rust|runtime|ci-runtime|cli|python]     (default: all)
# C/native regressions need gcc; Python checks need Python 3.11+. Runtime tests build native modules.
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
FAILURES=0
mkdir -p target
if [[ $what == abi || $what == all ]]; then
    python3 tests/test_dependencies.py
fi
if [[ $what == generated || $what == abi || $what == all ]]; then
    python3 scripts/check-module-memory.py
    # regenerate and compare: a stale generated file is reported (and left regenerated). gen.sh needs cbindgen and mesh-api-gen
    # (on PATH, both come from the flake); without them the check is skipped, the generated files are committed.
    if command -v cbindgen >/dev/null && command -v mesh-api-gen >/dev/null; then
        sums() { find api/generated -type f | LC_ALL=C sort | xargs sha256sum; sha256sum include/xip.h modules/*/API.manifest crates/sys/src/tables.rs crates/xip-abi/src/schema_data.rs crates/xip-abi/src/vt_shims.rs crates/xip-abi/src/ble_vtable.rs modules/mod_ble/src/api_gen.rs crates/xip-abi/src/nan_vtable.rs modules/mod_nan/src/api_gen.rs modules/mod_nan/src/power_gen.rs crates/xip-abi/src/sys_vtable.rs crates/sys/src/api_gen.rs crates/xip-abi/src/lora_vtable.rs modules/mod_lora/src/api_gen.rs; }
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
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -o target/test_runtime_range tests/host/test_runtime_range.c
    target/test_runtime_range
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -o target/test_module_panic tests/host/test_module_panic.c
    target/test_module_panic
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include -o target/test_module_alloc tests/host/test_module_alloc.c components/xip_runtime/src/xip_pool.c
    target/test_module_alloc
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -o target/test_port_bind tests/host/test_port_bind.c
    target/test_port_bind
    # the envelope parser, the answer head and the table of waiting requests of the runtime's envelope handling
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -o target/test_envelope tests/host/test_envelope.c components/xip_runtime/src/xip_envelope.c
    target/test_envelope
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -o target/test_activity tests/host/test_activity.c components/xip_runtime/src/xip_envelope.c
    target/test_activity
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -I tests/host/fake_idf -o target/test_serial_wake tests/host/test_serial_wake.c components/xip_runtime/src/xip_serial_wake.c
    target/test_serial_wake
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
    # which image of a name runs (versions, shadowed pages, RAM cells)
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_loader/include \
        -o target/test_select tests/host/test_select.c components/xip_loader/src/xip_select.c
    target/test_select
    # the runtime image rules shared by Stage2 and the loader (slots, versions, trial)
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I components/xip_loader/include -o target/test_image tests/host/test_image.c
    target/test_image
    # the extent scan shared by Stage2, the loader and (through the loader) mod_flash
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_loader/include -o target/test_extent tests/host/test_extent.c
    target/test_extent
    # the packet pool
    gcc -Wall -Wextra -fsanitize=address,undefined -g -I components/xip_runtime/include -o target/test_pool tests/host/test_pool.c \
        components/xip_runtime/src/xip_pool.c -lpthread
    target/test_pool
    # the PPP codec of the serial line, on a fake pool
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include -I tests/host/fake_idf -I tests/host \
        -o target/test_ppp tests/host/test_ppp.c components/xip_runtime/src/xip_ppp.c
    target/test_ppp
    # the QUIC classifier, relay table and envelope fields of the common QUIC receive function (golden packets from quic-lite)
    gcc -Wall -Wextra -Werror -fsanitize=address,undefined -g -I include -I components/xip_runtime/include \
        -o target/test_quic tests/host/test_quic.c components/xip_runtime/src/xip_quic.c
    target/test_quic
    # NOW framing native tests remain local to crates/xip-runtime-esp/tests/.
fi
if [[ $what == ci-runtime || $what == all ]]; then
    [[ $what != ci-runtime ]] || python3 tests/test_partition_layout.py
    cargo test --quiet --locked --manifest-path crates/Cargo.toml -p xip-abi --features global-alloc --lib heap::tests
fi
if [[ $what == rust || $what == runtime || $what == ci-runtime || $what == all ]]; then
    # the crates and the Rust modules: logic on a PC (the images are built by scripts/build.sh with the esp toolchain)
    module_crates=(modules/mod_system modules/mod_lora modules/mod_nan modules/mod_disc modules/mod_quic)
    module_crates+=(modules/mod_flash)
    RUSTC_BOOTSTRAP=1 cargo test --quiet --manifest-path modules/mod_rtest/Cargo.toml
    for crate in "${module_crates[@]}"; do
        cargo test --quiet --manifest-path "$crate/Cargo.toml"
    done
    # the host-runtime runs the modules (built for this machine) against the host table: virtual time, fixtures
    scripts/build-host-modules.sh >/dev/null
    # crates/: one workspace, one Cargo.lock (xip-abi, both runtimes, dmesh-cli)
    cargo test --quiet --locked --manifest-path crates/Cargo.toml --workspace --exclude dmesh-cli
fi
if [[ $what == rust || $what == cli || $what == all ]]; then
    # Collect failures without hiding them behind a presumed dependency diagnosis.
    # Integration targets compile here; hardware cases remain #[ignore].
    if ! cargo test --quiet --locked --manifest-path crates/Cargo.toml -p dmesh-cli; then
        echo "FAILED: dmesh-cli checks (see failure above)" >&2
        FAILURES=1
    fi
fi
if [[ $what == python || $what == all ]]; then
    python3 -m unittest discover -s tests -p 'test_*.py'
fi
if [[ $FAILURES != 0 ]]; then
    echo "test-host: requested groups finished with failures above" >&2
    exit 1
fi
