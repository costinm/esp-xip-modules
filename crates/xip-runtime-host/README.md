# xip-runtime-host

Run and test modules on Linux using the same module API as the ESP runtimes.

From the repository root:

```sh
. ./env.sh
scripts/build-host-modules.sh
cargo run --manifest-path crates/xip-runtime-host/Cargo.toml --bin xip-host
```

This starts the console on `127.0.0.1:5226` and an async echo module on `127.0.0.1:5228`. Both bind TCP and UDP. The console network/stdin interface is retained text compatibility; binary control tests use the module request API directly. The host executable does not yet expose the firmware's generic envelope router on those sockets.

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

Run the maintained host checks with `scripts/test-host.sh generated` followed by `scripts/test-host.sh ci-runtime`. The local legacy `all` group also uses root `tests/` files and is not used by CI. Tests can also load modules with `Runtime::load_module`, bind their declared ports with `Instance::listen_network`, and run them with `Runtime::run_realtime`. For hardware fixtures and deterministic time, use `Runtime::run(ms)`; see [the tests](tests) for examples.

For structured Rust assertions, load the console through `Runtime::load_module` and use `Instance::request(20, 5, &[0xa0])` for status, then decode the CBOR fields with `xip_abi::cbor`. Console method 4 accepts a correlated envelope for console/native calls; other modules can be addressed directly through their instance request API. See [console tests](tests/console.rs), [lifecycle tests](tests/network.rs) and [the binary protocol guide](../../notes/docs/protocol.md). New tests should not generate command lines or scrape text responses.
