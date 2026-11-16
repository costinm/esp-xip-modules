# `mux` API (52)

Standalone `quic-mux` stream multiplexer endpoint over PPP serial frames
(port `245` / `0xf5`) and TCP sockets (stream port `4`, TCP port `5229`).

- **Owner association (`OWNER_ASSOC = AssociationId(1)`):** All PPP serial packets
  and all connected TCP streams on port `5229` share the single owner association
  (`AssociationId(1)`). A single TCP stream or PPP link is never multiplexed across
  multiple associations; instead, the owner association bonds multiple TCP streams
  and the PPP serial link (`TCP` preferred at lower cost, `serial` failover).
- **PPP serial (`--port 245`):** Each PPP record is directly a raw `quic-mux` packet
  (`[QUIC frames...]`, `<= 1,500` bytes) with no outer association header.
- **TCP (`--stream 4`, port `5229`):** Byte stream carrying self-delimiting raw
  QUIC frames directly (`STREAM` frames with the `0x02` `LEN` bit + varint length,
  plus control frames) with no outer packet wrapper.
- **Stream dispatch:** Accepted streams feed into the native runtime `stream`
  component (`xip_stream`), which parses the opening CBOR request envelope and
  dispatches to the target consumer module.

## 1. `status` — endpoint state

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `associations` | `uint` | Active `quic-mux` associations. |
| 2 | `streams` | `uint` | Active multiplexed streams across all associations. |
| 3 | `tcp_sockets` | `uint` | Connected TCP bearer sockets on stream port 4. |
| 4 | `received_frames` | `uint` | Frames admitted from serial and TCP bearers. |
| 5 | `sent_frames` | `uint` | Frames submitted to serial and TCP bearers. |
| 6 | `coalesced_packets` | `uint` | Outbound small frames coalesced into pending TCP packets. |
| 7 | `errors` | `uint` | Local framing or stream errors observed. |
| 8 | `last_error` | `int` | Most recent native/module error code (`0` if none). |

## 2. `reset` — discard endpoint associations

No arguments. Closes active stream sessions, closes connected TCP bearer sockets,
and clears all `quic-mux` association state. Returns an empty map.
