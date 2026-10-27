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
dmesh-cli console "$PORT" quic.status
```

Enable optional transport counters before a test, then read the last snapshot:

```sh
dmesh-cli "$PORT" quic.stats --enabled=true
dmesh-cli "$PORT" quic.stats
dmesh-cli "$PORT" quic.stats --enabled=false
```

See [API.md](API.md) for control methods and [the upload commands](../../notes/ai/quic/quic_flash.md) for UART/USB and UDP flashing.

The endpoint uses the runtime's shared packet pool through generic Host ABI 4.
It owns received packets and retained protocol history until consumption or
close. Accepted TX holds a separate driver reference until physical use ends.
There are no endpoint packet quotas, ticket tables or return-packet completion
queues; TX BUSY keeps the prepared packet for another path or retry.

## Retained state

The 8-byte state block contains a running-context pointer. A started endpoint
retains its native handles, transport state and enabled statistics; the core
owns association/stream state and packet history while those connections need
it. Packets use the shared native pool, not a private packet-storage array.
Statistics are optional and reclaimed when disabled. Stream close releases
session resources, but a started endpoint and association capacity can remain
resident for reuse. Stop drops bridges, sessions, core and context and closes
native handles. An installed, unstarted image has no endpoint instance.

Verify the declared state/stack budget with `python3 scripts/check-module-memory.py`
from the repository root. For device heap/packet lifetime measurements, use the
[common measurement procedure](../README.md#measuring-retained-ram).
