# xip-runtime-esp

The Rust side of an esp-xip-modules application: start the host and the loader over the module partition (it scans the partition: any module image on a 64 KB page is found, named by its manifest), start, stop and call
modules. It moves bytes and knows nothing else: no module (a module's operations, field ids and events
are its own crate: `esp-mod-lora`, `esp-mod-hw`), no transport and no CBOR.

```rust
xip_runtime::init("data\0", Features::default(), None);                           // host worker + loader, scans the partition
let n = xip_runtime::call("lora", b"idle", 43, method, &request, &mut answer)?;    // start it if needed, send, take the answer
```

* `Features { modules }` chooses whether the module list and lifecycle calls are exposed to modules.
* Feature `console` adds `console::start()`: the console module (`modules/mod_con`), which owns the serial port.
* `/main-rs` (repository root) is the Rust main, twin of `main/main.c`; `main-rs/build.sh esp32c6` builds it with the ESP Rust toolchain. It calls `autostart()` last.

The C it needs is in `components/` (`xip_runtime` and `xip_loader`): give the build those directories as extra component dirs.

The Rust host owns the shared lwIP poller (the socket events of the manifest's stream ports); flash ABI calls are declared but their event bridge is still pending; signed upload policy belongs in an optional no-std `mod_flash` module.

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
