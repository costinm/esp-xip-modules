# QUIC module

A QUIC-lite stream endpoint for the C runtime. The runtime dispatches each
stream from its opening CBOR request envelope; QUIC carries the service bytes.
`mod_flash` accepts the same upload packages over TCP or QUIC streams. The
module reserves slots 9–12 and is not autostarted.

This version is **plaintext and unauthenticated**. Crypto will be in the C runtime,
shared with all other protocols and packets - for the data frames. The module may
do a handshake to set the secret for a connection - but not yet implemented.

Start it on a device with an enabled network link:

```sh
dmesh-cli console "$PORT" console.start --name=quic
dmesh-cli console "$PORT" quic.status --port 244
```

See [API.md](API.md) for control methods and [the upload commands](../../notes/docs/quic_flash.md) for UART/USB and UDP flashing.
