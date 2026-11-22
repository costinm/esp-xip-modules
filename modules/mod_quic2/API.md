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

## 3. `config` — configure optional encryption and keys

All fields are optional; omitted fields preserve their current value. Uses native
runtime PSA Crypto (`xip_crypto`), with per-link granularity: by default Wi-Fi/UDP/NOW
encryption (`encrypt`) is enabled while PPP serial (`serial_encrypt`) and TCP
(`tcp_encrypt`) are disabled, and any bearer can be toggled independently for
testing.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `encrypt` | `bool` | Enable (`true`) or disable (`false`) encryption on the Wi-Fi/UDP/NOW link (`LINK_UDP`). |
| 2 | `node_secret` | `bytes` | 32-byte device secret (`empty` clears it). |
| 3 | `api_key` | `bytes` | 32-byte shared key (`empty` clears it). |
| 4 | `serial_encrypt` | `bool` | Enable (`true`) or disable (`false`) encryption on the PPP serial link (`LINK_SERIAL`, default `false`). |
| 5 | `tcp_encrypt` | `bool` | Enable (`true`) or disable (`false`) encryption on TCP bearer links (`LINK_TCP_BASE+idx`, default `false`). |
| 6 | `cipher` | `uint` | Select AEAD cipher (`0 = AES-128-GCM` default, `1 = AES-128-CCM`). |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `encrypt` | `bool` | Current Wi-Fi/UDP/NOW encryption setting. |
| 2 | `keys_active` | `bool` | Whether association AEAD keys or `node_secret` are configured. |
| 3 | `serial_encrypt` | `bool` | Current PPP serial encryption setting. |
| 4 | `tcp_encrypt` | `bool` | Current TCP bearer encryption setting. |
| 5 | `cipher` | `uint` | Active AEAD cipher (`0 = AES-128-GCM`, `1 = AES-128-CCM`). |
