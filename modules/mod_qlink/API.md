# `qlink` API (54)

Standalone single-stream `quic-link` endpoint (`SingleStreamEndpoint` without
`quic-mux`) over UDP (`hw.quic` Short-Header classifier and datagram port `242`)
and PPP serial frames (`--port 242` / `0xf2`).

- **Single reliable stream + datagrams:** Provides one reliable ordered stream
  (with Selective ACK `0x02`, packet-number reconstruction, per-link congestion
  control, and `MAX_DATA` flow control) plus RFC 9221 unreliable `DATAGRAM`s
  (`0x30`/`0x31`) without `quic-mux` multi-stream table overhead.
- **Bearers:** Bonds UDP (`LinkId(1)`, cost tier `0`) and PPP serial (`LinkId(2)`,
  `--port 242`, cost tier `1`).
- **Stream dispatch:** Feeds the single reliable stream into the native runtime
  `stream` component (`xip_stream`), automatically resetting stream state after
  completion (`rx_fin && tx_fin`) so sequential requests work across consumer
  modules (`mod_rtest`, `mod_flash`, etc.).

## 1. `status` — endpoint state

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `active_stream` | `bool` | Whether a native stream session is currently open. |
| 2 | `received_packets` | `uint` | Packets admitted from UDP and PPP serial bearers. |
| 3 | `sent_packets` | `uint` | Packets submitted to UDP and PPP serial bearers. |
| 4 | `retained_tx` | `uint` | Unacknowledged TX packets currently retained in the SACK ledger. |
| 5 | `errors` | `uint` | Local framing or stream errors observed. |
| 6 | `last_error` | `int` | Most recent native/module error code (`0` if none). |

## 2. `reset` — discard endpoint state

No arguments. Closes any active stream session and resets the `SingleStreamEndpoint`
and its SACK ledger. Returns an empty map.
