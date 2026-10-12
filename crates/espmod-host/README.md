# espmod-host

The Rust side of an esp-xip-modules application: start the host and the loader over a slot table, start, stop and call
modules, take their events. It moves bytes and knows nothing else: no module (a module's operations, field ids and events
are its own crate: `esp-mod-lora`, `esp-mod-hw`), no transport and no CBOR.

```rust
static SLOTS: [Slot; 2] = [slot!("lora", 43, 0x00000, 0x10000), slot!("hw", 45, 0x10000, 0x10000)];

espmod_host::init(&SLOTS, "data\0", Features::default(), Some(on_event));        // host worker + loader
let n = espmod_host::call("lora", b"idle", 43, method, &request, &mut answer)?;    // start it if needed, send, take the answer
```

* `slot!` is the table `tools/mkmod.py --slots` reads (the file may be Rust).
* `Features { modules, network }` choose what the ABI offers beyond the hardware: the module list for the console, and Wi-Fi +
  uploads on esp-wifi-loader's C component (feature `network`). An application with its own network and installer
  (DMesh) passes neither.
* Feature `console` adds `console::start()`: the console module (`modules/mod_console`), which owns the serial port.
* `example/` is the Rust twin of `main/main.c`; `example/build.sh esp32c6` builds it with the ESP Rust toolchain.

The C it needs is in `components/` (`espmod_host`, `espmod_loader`, `espmod_install` and `modules/mod_console`): give the
build those directories (and `esp-wifi-loader/components` for `network`) as extra component dirs.
