# mod_system

One module, named `sys`, for two components. It has no autostart: a request for one of its components, or a `get_component` of a table it hosts, activates it.
Each component is a crate under `crates/`, and this module only composes them.

| Component | Crate | API |
|---|---|---|
| 49 `sys` | `crates/sys` | [API.md](../../crates/sys/API.md) |
| 45 `hw` | `crates/hw` | [API.md](../../crates/hw/API.md) |

Device control (module lifecycle, status, reset, saved settings) is deprecated: its methods remain available during the split (the planned
owners are in the [sys API](../../crates/sys/API.md)). The hardware part is deprecated too; do not install it on new devices.

## System diagnostics

Use `dmesh-cli DEVICE sys.heap` for memory usage, `sys.heap_dump --kind=0`
for regions, `--kind=1` for blocks, and `sys.dump_tasks` for tasks and stack
usage. Heap trace kinds are 3 (start), 4 (dump), 5 (stop); kind 6 streams CBOR
block records. See [API.md](API.md).

The installed module exposes its generated native table and remote methods. Power
information, sleep and PM-lock diagnostics remain in NAN (`nan.dump_locks`).

## Hardware policy

Battery level behind a voltage divider, an ADC probe and a GPIO button with short,
long and double press events. Settings (`battery.pin`, `button.gpio`, ...) supply the
fields a request omits; events are CBOR arrays. See [hw API](../../crates/hw/API.md).

## Device control

Module lifecycle, status, reset and saved settings are methods of the sys component. It has no port: requests arrive as
envelopes in QUIC packets (UDP 3339 by default, PPP port 0xf7, `ble`, `espnow`) and the runtime dispatches them. Binary
clients call `sys.status`, `start`, `stop`, `reset` and `nv`. Native Wi-Fi state and nonblocking scan results are encoded
here from the host Wi-Fi table. See [sys API](../../crates/sys/API.md) for fields and responses.

```sh
dmesh-cli DEVICE sys.status
dmesh-cli DEVICE sys.start --name=nan
dmesh-cli DEVICE sys.stop --name=disc
```

Build it with `scripts/build.sh esp32c6` from the repository root. Firmware must
restart to publish newly installed methods. Use
[dmesh-cli](../../crates/dmesh-cli/README.md) to access a serial or network device.
