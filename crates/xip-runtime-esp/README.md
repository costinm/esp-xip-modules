# xip-runtime-esp

Rust integration for ESP firmware using the C module runtime and loader. Use it
to initialize the host, load installed modules, and start, stop or call them.

```rust
use xip_runtime_esp::{autostart, init, Features};

assert_eq!(init("data\0", Features { modules: true, ..Features::default() }, None), 0);
autostart();
```

Add `components/xip_runtime` and `components/xip_loader` to the ESP-IDF component
paths. See [main-rs](../../main-rs/src/main.rs) for a complete application.
Applications that manage their own radio supply a `WifiOwner` in `Features`.

Build from the repository root:

```sh
. ./env.sh
main-rs/build.sh esp32c6
```

Use the ESP toolchain selected by `env.sh`.

See [the C runtime](../../components/xip_runtime/README.md) and
[the ABI](../xip-abi/README.md).
