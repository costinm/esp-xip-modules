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

## Desired bearers

`profile` owns the shared ephemeral bearer store and atomic partial requests.
Its types and CBOR codecs come from `xip-abi::profile`, generated from
[`abi/API.md`](../../abi/API.md). Products use this Rust API directly; modules
use CBOR `transport_set` and `transport_get` in the host ABI. See
[`xip-abi::profile`](PROFILE.md) for publication and owner rules.

`settings` owns the ESP NVS access used by the Rust Wi-Fi initializer.
The Rust driver calls ESP-IDF directly and does not depend on `xip_wifi`'s
C helper implementation. The optional C Wi-Fi component uses the same `wl`
memory settings and can reconcile the STA subset of the desired profile.
