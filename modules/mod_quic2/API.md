# `quic2` API (55)

Combined `quic-link` (`QuicLinkAssoc`) + `quic-mux` (`QuicMux`) multi-link
endpoint over UDP (`hw.quic` Short-Header classifier and datagram port `243`),
PPP serial (`--port 243` / `0xf3`), and TCP sockets (`--stream 6`, TCP port `5231`).

- **Owner association (`OWNER_ASSOC = AssociationId(1)`):** Multiplexes concurrent
  streams (`QuicMux`) over a single `QuicLinkAssoc` bonding TCP (`LinkId(10 + idx)`),
  UDP (`LinkId(1)`), and PPP serial (`LinkId(2)`).
- **Packet framing:** Every bearer carries QUIC `ShortHeader` packets with
  association-wide packet numbers, Selective ACK (`0x02`) piggybacking, per-link
  congestion control, and zero-copy cross-link SACK retransmission. On TCP byte
  streams (`--stream 6`), packets are framed via `plan_short_header_stream_packet`
  (`ShortHeader` + self-delimiting `ACK`/control frames + length-prefixed `STREAM`
  frames).
- **Stream dispatch:** Feeds accepted streams into the native runtime `stream`
  component (`xip_stream`), which parses the opening CBOR request envelope and
  dispatches to the target consumer module (`mod_rtest`, `mod_flash`, etc.).

## 1. `status` — endpoint state

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `associations` | `uint` | Active `quic-mux` associations (`1` when active). |
| 2 | `streams` | `uint` | Active multiplexed streams. |
| 3 | `tcp_sockets` | `uint` | Connected TCP bearer sockets on stream port 6. |
| 4 | `received_packets` | `uint` | Packets admitted across UDP, PPP serial, and TCP. |
| 5 | `sent_packets` | `uint` | Packets submitted across UDP, PPP serial, and TCP. |
| 6 | `retained_tx` | `uint` | Unacknowledged TX packets currently retained in the SACK ledger. |
| 7 | `errors` | `uint` | Local framing or stream errors observed. |
| 8 | `last_error` | `int` | Most recent native/module error code (`0` if none). |

## 2. `reset` — discard endpoint state

No arguments. Closes active stream sessions, closes connected TCP bearer sockets,
and resets `QuicMux` and `QuicLinkAssoc`. Returns an empty map.
