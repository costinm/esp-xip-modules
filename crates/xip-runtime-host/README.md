# xip-runtime-host

The xip runtime for a development host (Linux). It is a real runtime, like the ESP ones: it loads modules, gives each an
instance, delivers start/stop, requests, packets, alarms and interrupts one call at a time, and implements the host table of
`include/xip.h`. What differs from a device is what the host can afford: virtual time, fixtures for the hardware, traces and
checks. It replaces the C simulator that used to live in `tests/host`.

```sh
scripts/build-host-modules.sh        # the modules as shared objects, in target/host-modules
cargo test --manifest-path crates/xip-runtime-host/Cargo.toml
```

```rust
let rt = xip_runtime_host::Runtime::default();
let m = rt.add_module("target/host-modules/mod_rtest.so", 64)?;   // state bytes, as the image header asks
m.start(b"selftest");
rt.run(100);                                                      // 100 ms of virtual time
assert!(m.ended() && m.find_event(0x7eff, "fail=0").is_some());
```

* **Modules** are native shared objects: the same C and Rust sources that become flash images, built for this machine. Each
  instance gets its own copy of the object (its own statics). A handler linked into the process works too (`add_handler`).
* **Time** is virtual. `Runtime::run(ms)` runs queued calls, then alarms in time order; nothing sleeps. Wake windows
  (`set_window`) and connectivity (`set_connected`) follow the alarm rules of the device runtimes.
* **Pure Rust.** No C is compiled into the crate (the modules are, by `scripts/build-host-modules.sh`). Packets, held
  packets and the memory balance are a port of the C runtime's pool (`pool.rs`: slots with reference counts and a reserve
  for the application, handles with generations, owned-packet and byte quotas), with the same unit cases.
* **Pins and SPI** are fixtures: `wire` joins two pins, `drive` changes one, `set_spi_device(cs, device)` attaches a device
  to a chip-select. `fixtures` has a loopback, a scripted device, `Recorder` (log every transfer) and `Replay` (play a recording
  back and report where the module behaves differently). Recordings are text, `t=<ms> tx=<hex> rx=<hex>`, so a capture from
  a real board can be edited and diffed.
* **Observing**: events, log lines, packets sent to the application (`state().main`), the serial output, and
  `set_trace(true)` / `take_trace()` for every handler call and host call.
* `register_component(name, table)` is what `get_component(name)` returns (the `psa` table of a software provider goes here).

Not here yet: sockets and flash (they answer `ERR_UNSUPPORTED`), real-time operation with
a worker thread, loading a `.dmod` image (the host cannot run RISC-V or Xtensa code; the same sources are built natively).
