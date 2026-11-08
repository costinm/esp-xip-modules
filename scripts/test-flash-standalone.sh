#!/usr/bin/env bash
# Real mod_flash Wasm, one module, a persistent 4 MiB NOR file, and dmesh-cli TCP.
set -euo pipefail
cd "$(dirname "$0")/.."
cli=${DMESH_CLI:-$PWD/crates/target/debug/dmesh-cli}
host=${XIP_HOST:-$PWD/crates/target/debug/xip-host}
work=${XIP_FLASH_TEST_DIR:-$PWD/target/flash-standalone-test}
base=${XIP_FLASH_TEST_BASE:-15225}
mkdir -p "$work"
pid=""
cleanup(){ if [[ -n $pid ]];then kill -INT "$pid" 2>/dev/null || true; wait "$pid" || true;pid="";fi; }
trap cleanup EXIT
rm -f "$work/flash.bin"
python3 tools/xipfs.py init "$work/flash.bin" --size 4
python3 - "$work" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]);p.joinpath('input.bin').write_bytes((bytes(range(251))*240)[:60001])
p.joinpath('log.cbor').write_bytes(bytes.fromhex('a201182a021903e8'))
PY
start(){
    XIP_THREADS=1 XIP_BASE_PORT="$base" XIP_QUIC_PORT=off XIP_WIFI_SSID=costin \
        XIP_FLASH_FILE="$work/flash.bin" XIP_FLASH_RUNNING=0x10000:0x180000 \
        "$host" target/host-modules/mod_flash.wasm >> "$work/host.log" 2>&1 &
    pid=$!
    for _ in {1..50};do
        if "$cli" "tcp://127.0.0.1:$((base+2))" flash.fs.info > "$work/info.json" 2>/dev/null;then return;fi
        kill -0 "$pid";sleep .1
    done
    cat "$work/host.log" >&2;return 1
}
call(){ "$cli" "tcp://127.0.0.1:$((base+2))" "$@"; }
: > "$work/host.log"
start
"$cli" flash-discover "127.0.0.1:$((base+2))"
call flash.file.write --name=tcp-test --input="$work/input.bin" --chunk=3
call flash.file.read --name=tcp-test --output="$work/read.bin"
cmp "$work/input.bin" "$work/read.bin"
call flash.log.append --first_seq=1 --count=1 --input="$work/log.cbor" --chunk=1
call flash.file.sync
cleanup
start
call flash.file.stat --name=tcp-test
call flash.file.read --name=tcp-test --output="$work/reopened.bin"
cmp "$work/input.bin" "$work/reopened.bin"
call flash.file.write --name=tcp-test --input="$work/input.bin"
call flash.file.remove --name=tcp-test
if call flash.file.stat --name=tcp-test > "$work/removed-stat.log" 2>&1;then echo 'FAIL: removed file found' >&2;exit 1;fi
rg -q 'flash error -7' "$work/removed-stat.log"
[[ $(stat -c %s "$work/flash.bin") == 4194304 ]]
echo 'PASS: standalone mod_flash TCP, discovery, fragmented file/log writes, restart, replacement and removal'
