# xip-runtime-esp

The Rust side of an esp-xip-modules application: start the host and the loader over the module partition (it scans the partition: any module image on a 64 KB page is found, named by its manifest), start, stop and call
modules. It moves bytes and knows nothing else: no module (a module's operations, field ids and events
are its own crate: `esp-mod-lora`, `esp-mod-hw`), no transport and no CBOR.

```rust
xip_runtime::init("data\0", Features::default(), None);                           // host worker + loader, scans the partition
let n = xip_runtime::call("lora", b"idle", 43, method, &request, &mut answer)?;    // start it if needed, send, take the answer
```

* `Features { modules }` chooses whether the module list and lifecycle calls are exposed to modules.
* The C runtime owns the serial driver. The installed `modules/mod_con` handles its logical console port; DMesh supplies its own packet codec over that same C driver/task owner.
* `/main-rs` (repository root) is the Rust main, twin of `main/main.c`; `main-rs/build.sh esp32c6` builds it with the ESP Rust toolchain. It initializes the host/loader, applies the shared C boot-network fallback and calls `autostart()` last. Its build uses the same SDK baseline as C Main.
* `/fw/main` is DMesh's Rust main (with `fw/transport` and `fw/ble`); `fw/build.sh esp32c6` builds it (uses pinned portable DMesh Git dependencies). It calls `init` and `autostart()` early, before the transport starts: its startup differs from `main-rs` (see notes `ai/2026-10-18-dmesh-fw-move.md`).

The C it needs is in `components/` (`xip_runtime` and `xip_loader`): give the build those directories as extra component dirs.

Both generic C and Rust hosts use the C lwIP readiness task for module sockets
(`components/xip_runtime/src/xip_lwip_io.c`). The C registry provides
generation-checked registrations, periodic ticks, suspend/rearm and deferred
TCP listener binding. The task and registry are allocated on first use and
reclaimed after the final registration is cleared. Module handlers run on the
host worker. DMesh QUIC uses its raw UDP6 bearer; the optional native TCP and
lwIP UDP comparison backends and their Rust poller bindings have been removed.
Flash ABI calls are declared but their event bridge is still pending; signed
upload policy belongs in an optional no-std `mod_flash` module.

## Wi-Fi

Modules see Wi-Fi through the `wifi` and `wifi-tuning` components
(`get_component`; `crates/xip-abi/src/wifi.rs`). Generic C and Rust firmware use
the same C intended-state store, generation notifications, radio reconciliation,
scanning, tuning and sleep hooks. Boot credentials come from `dmesh/sta_ssid` and
`sec/sta`; defaults and startup behavior are those of the C owner.

An application that drives the radio itself passes `Features { wifi: Some(&OWNER), .. }`.
`WifiOwner` is only the Rust hook binding type. In that case the runtime installs
no generic radio or sleep owner. DMesh retains its profile, driver initialization
and radio policy under `fw/transport`; its init-config code and NOW wire bindings
live there, and profile changes notify modules through `xip_host_wifi_changed`.
There is no second generic Rust Wi-Fi store or reconciler.

The generic C and Rust applications share that C radio/NOW owner and its validated
`xip_wifi_owner_monitor` entry. Promiscuous callbacks, filtering and pool delivery
remain entirely in `xip_link_promisc.c`; no Rust monitor wrapper is needed.
Promiscuous frame capacity follows `PKT_MAX`.

The former Rust
NOW driver and codec have been removed; DMesh's `fw/transport` retains its product
transport owner. `now = 0` leaves the NOW link disabled. Native driver regressions
run with `cargo test --manifest-path crates/Cargo.toml -p xip-runtime-esp`; they
compile the actual C driver and pool against SDK boundary fakes and use the
crate's raw-wire vectors (also checked by DMesh's portable peer), without the local root `tests/` tree.

NOW uses a 16-byte action/vendor-IE prefix followed by the opaque payload; the
receiver skips the IE. The default 144-byte packet window sends in place when
uniquely owned. NOW RX admits only prepared pool storage and drops on cache
exhaustion. External `PoolOps` may supply nonallocating `acquire_cached`; absent
that hook, cached admission refuses the packet. See the native runtime's
[ESPNOW.md](../../components/xip_runtime/ESPNOW.md) for ownership and LR tuning.

FW's UART adapter also uses the shared C PPP codec and one-current-frame TX
owner. Rust bridges its typed packet pool and QUIC completion token through the
generated `SerialPacketClient` contract; it has no UART decoder/encoder or TX
packet queue. Busy admission retains the exact packet at the caller.
