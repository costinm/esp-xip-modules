# xip-runtime-host

Run and test modules on Linux/Android using the same module API as the ESP runtimes.

From the repository root:

```sh
. ./env.sh
scripts/build-host-modules.sh
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host
```

This loads `mod_system` (the sys component has no port; it serves the request envelopes the runtime dispatches to it) and `mod_rtest` with an async
echo on TCP/UDP `127.0.0.1:5228`. Requests arrive over QUIC, as on the devices: `dmesh-cli` wraps every request in a `0x444d0002`
long packet, and the runtime's classifier (`quic.rs`, a Rust port of the C runtime's, with no code shared) takes it from the link
`udp`, a socket on the QUIC port. The port is `XIP_QUIC_PORT` (default 3340, `0` any, `off` none). It is not the device port 3339, which lmesh and the firmware use (`dmesh-cli` itself listens on 3338); the
startup line prints the port.

```sh
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host &
dmesh-cli console udp://127.0.0.1:3340 sys.status
```

There is no text/stdin console. Rust tests call modules through the request API directly (see below); the
[sys API](../../crates/sys/API.md) describes the envelopes. Short packets and other long versions go to the QUIC stack
registered with `Runtime::set_quic_local` (none in `xip-host` yet, so they are dropped and counted).

Try the echo service (raw bytes, not a request):

```python
import socket

with socket.create_connection(("127.0.0.1", 5228)) as stream:
    stream.sendall(b"hello")
    print(stream.recv(5))

with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as datagram:
    datagram.settimeout(2)
    datagram.sendto(b"hello", ("127.0.0.1", 5228))
    print(datagram.recv(100))
```

To choose modules and their start requests, pass `PATH.wasm[=START]` arguments. For example, `target/host-modules/mod_system.wasm target/host-modules/mod_rtest.wasm=async`. Keep each generated `.json` file beside its module. Restart `xip-host` to restart a stopped module.

Run checks with `scripts/test-host.sh generated` and `scripts/test-host.sh ci-runtime`.

To test a module from Rust, use `Runtime::load_module`, `Instance::request` and
`Runtime::run_realtime`. `Runtime::run(ms)` provides deterministic time for
fixtures. See [sys examples](tests/sys.rs) and [network examples](tests/network.rs).
