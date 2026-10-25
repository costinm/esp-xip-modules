# `quic` API (48)

Optional experimental QUIC-lite echo endpoint for the C runtime. Start it through
the module manager; it receives unhandled local QUIC packets on native links.
Control requests are normal compact CBOR envelopes in 0x444d0002 datagrams (PPP port 0xf7 for dmesh-cli); PPP port 244 carries raw QUIC frames. No start
parameters. This version is plaintext and unauthenticated: use trusted test peers.
It does not implement a TCP proxy or an IP tunnel.

## 1. `status` — endpoint state

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `associations` | `uint` | Currently admitted associations; limit one. |
| 2 | `allocated` | `bool` | Lazy QUIC core exists. |
| 3 | `received` | `uint` | Frames admitted to the module handler. |
| 4 | `errors` | `uint` | Local processing errors. |
| 5 | `last_error` | `int` | Last native/module error. |
| 6 | `protected` | `bool` | Always false in this experimental endpoint. |
| 7 | `stream_limit` | `uint` | Two streams per association. |

## 2. `reset` — discard endpoint associations

No arguments. Returns an empty map after detaching the endpoint, disposing its
QUIC state and reattaching. Peers must reconnect. Already accepted native TX is
allowed to complete; its buffers are reclaimed by C without calling the old
module. Stopping or updating the module has the same ownership guarantee.
