# xip-runtime-host

Run and test modules on Linux using the same module API as the ESP runtimes.

From the repository root:

```sh
. ./env.sh
scripts/build-host-modules.sh
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host
```

This starts the CBOR console on UDP `127.0.0.1:5226` and an async echo module
on TCP/UDP `127.0.0.1:5228`. Send numeric console envelopes to the UDP port;
see the [console API](../../modules/mod_con/API.md). There is no text/stdin
console. Arbitrary module routing through the console is not supported;
Rust clients can use the module request API directly.

Try the echo service:

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

To choose modules and their start requests, pass `PATH.so[=START]` arguments. For example, `target/host-modules/mod_con.so target/host-modules/mod_rtest.so=async`. Keep each generated `.json` file beside its module. Restart `xip-host` to restart a stopped module.

Run checks with `scripts/test-host.sh generated` and `scripts/test-host.sh ci-runtime`.

To test a module from Rust, use `Runtime::load_module`, `Instance::request` and
`Runtime::run_realtime`. `Runtime::run(ms)` provides deterministic time for
fixtures. See [console examples](tests/console.rs) and [network examples](tests/network.rs).
