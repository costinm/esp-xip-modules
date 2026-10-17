# xip-abi

The module ABI, declared in Rust. `src/host.rs` is the host table and its constants (`include/xip.h` is generated from it by
`scripts/gen.sh`); `src/wifi.rs` and `src/psa.rs` are components; `src/lib.rs` is the safe `Hw` wrapper Rust modules
use and the model of the ABI. The documentation is the doc comments: `cargo doc -p xip-abi`, or read `include/xip.h`.

The CBOR, JSON and text view of the same APIs (field tags and names, for consoles and gateways) is in `api/*.API.md`.
