# QUIC module (Deprecated)

> **Deprecated:** `mod_quic` (component `48`, built on the monolithic `quic-lite` crate and occupying link slots `9..12` with a 32 KiB stack) is deprecated and superseded by:
> - [`mod_mux`](../mod_mux/README.md) (component `52`, `quic-mux` over TCP `:5229` and PPP port `245`, 1 slot, 8 KiB stack)
> - [`mod_qlink`](../mod_qlink/API.md) (component `54`, single-stream `quic-link` over UDP and PPP port `242`, 1 slot, 8 KiB stack)
> - [`mod_quic2`](../mod_quic2/API.md) (component `55`, combined `quic-link` + `quic-mux` over UDP, PPP port `243`, and TCP `:5231`, 2 slots, 8 KiB stack)

A QUIC-lite stream endpoint for the C runtime. The runtime dispatches each
stream from its opening CBOR request envelope; QUIC carries the service bytes.
`mod_flash` accepts the same upload packages over TCP or QUIC streams. The
module is not autostarted.

This version is **plaintext and unauthenticated**. Crypto will be in the C runtime,
shared with all other protocols and packets - for the data frames. The module may
do a handshake to set the secret for a connection - but not yet implemented.

Start it on a device with an enabled network link:

```sh
dmesh-cli console "$PORT" sys.start --name=quic
dmesh-cli console "$PORT" quic.status
```

Enable optional transport counters before a test, then read the last snapshot:

```sh
dmesh-cli "$PORT" quic.stats --enabled=true
dmesh-cli "$PORT" quic.stats
dmesh-cli "$PORT" quic.stats --enabled=false
```

See [API.md](API.md) for control methods and [mod_flash](../mod_flash/README.md) for uploads.
