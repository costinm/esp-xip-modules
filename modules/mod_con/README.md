# mod_con

Debug and special handlers for module lifecycle, status, reset and saved
settings; not a console. It has no port: requests arrive as envelopes in QUIC
packets (UDP 3339 by default, PPP port 0xf7, `ble`, `espnow`) and the runtime
dispatches them. Binary clients call `console.status`, `start`, `stop`, `reset`
and `nv`. The runtime handles correlated envelope dispatch. Native
Wi-Fi state and nonblocking scan results are encoded here from the host Wi-Fi
table. See
[API.md](API.md) for fields and responses.

Build it with `scripts/build.sh esp32c6` from the repository root. Install it
with the other modules; its manifest requests startup at boot. Firmware must
restart to publish newly installed methods.

Use [dmesh-cli](../../crates/dmesh-cli/README.md) to access a serial or network device:

```sh
dmesh-cli DEVICE console.status
dmesh-cli DEVICE console.start --name=nan
dmesh-cli DEVICE console.stop --name=disc
```

Memory and task diagnostics live in mod_system: system.heap, system.heap_dump
and system.dump_tasks. PM-lock diagnostics remain nan.dump_locks.
