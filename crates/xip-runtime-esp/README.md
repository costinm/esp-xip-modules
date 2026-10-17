# xip-runtime-esp

The Rust side of an esp-xip-modules application: start the host and the loader over a slot table, start, stop and call
modules, take their events. It moves bytes and knows nothing else: no module (a module's operations, field ids and events
are its own crate: `esp-mod-lora`, `esp-mod-hw`), no transport and no CBOR.

```rust
static SLOTS: [Slot; 2] = [slot!("lora", 43, 0x00000, 0x10000), slot!("hw", 45, 0x10000, 0x10000)];

xip_runtime::init(&SLOTS, "data\0", Features::default(), Some(on_event));        // host worker + loader
let n = xip_runtime::call("lora", b"idle", 43, method, &request, &mut answer)?;    // start it if needed, send, take the answer
```

* `slot!` is the table `tools/mkmod.py --slots` reads (the file may be Rust).
* `Features { modules }` chooses whether the module list and lifecycle calls are exposed to modules.
* Feature `console` adds `console::start()`: the console module (`modules/mod_console`), which owns the serial port.
* `example/` is the Rust twin of `main/main.c`; `example/build.sh esp32c6` builds it with the ESP Rust toolchain.

The C it needs is in `components/` (`xip_runtime` and `xip_loader`): give the build those directories as extra component dirs.

The Rust host owns the shared lwIP poller and the `MODULE_SLOTS` layout preset. Socket and flash ABI calls are declared but their event bridge is still pending; signed upload policy belongs in an optional no-std `mod_flash` module.

## Wi-Fi

Modules see Wi-Fi through the `wifi` and `wifi-tuning` components (`get_component`; `crates/xip-abi/src/wifi.rs`). `wifi.rs`
keeps the wanted state (`WifiIntent`, `WifiTuning`, a generation), tells the modules when it changes (`HOST_WIFI`), and
`wifi_esp` is a simple station owner that makes it true on the host worker, starting from the boot credentials in NVS
(`dmesh/sta_ssid`, `sec/sta`, the keys `nv` writes). Every combination of the intent runs (ESP-NOW only, station, soft AP, both); the raw UDP6 tuning switches (`raw_rx`, `driver_tx`, `raw_tx_rate`) are
`ERR_UNSUPPORTED` until the `udp6` link is ported.

An application that drives the radio itself (DMesh) passes a `WifiOwner` in `Features { wifi: Some(&OWNER), .. }`: its
functions answer the components, and it calls `wifi::changed(generation)` after a change. The host then runs no owner and
reads no credentials. The application keeps its own policy and profile types; this crate has none.

`wifi_esp::init_config` / `init_config_with` build the driver's init config (the `wl` NVS buffer sizes, A-MPDU), the same
policy as the C host; `settings` is the NVS access.
