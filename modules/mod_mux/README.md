# Mux module

Multiplexed `quic-mux` stream endpoint over UART/USB PPP (port `245`) and TCP
(port `5229`). Accepted streams are handed to the native `stream` service, so
existing stream consumers (`rtest` probe `8.2`, `flash` uploads, and other CBOR
stream endpoints) work unchanged over multiplexed serial, TCP, or both links
together.

Start the module on a device:

```sh
dmesh-cli "$PORT" sys.start --name=mux
dmesh-cli "$PORT" mux.status
```

Reset active associations and connected TCP sockets:

```sh
dmesh-cli "$PORT" mux.reset
```

See [API.md](API.md) for control methods and wire framing.
