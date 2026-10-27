# `quic` API (48)

QUIC-lite module stream endpoint for the C runtime. Start it through
the module manager; it receives unhandled local QUIC packets on native links.
Control requests are normal compact CBOR envelopes in 0x444d0002 datagrams (PPP port 0xf7 for dmesh-cli); PPP port 244 carries raw QUIC frames. No start
parameters. Relay forwarding and routing belong to the native runtime. This version is plaintext and unauthenticated: use trusted test peers.
The endpoint passes stream bytes and FIN to the native runtime. The runtime
frames and parses the opening CBOR envelope and dispatches to its selected
consumer, using the same envelope parser and component ownership as datagrams.
`flash.upload` (44.1) accepts WLU1 packages; its response is the same upload
protocol response as TCP 5227.

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
| 7 | `stream_limit` | `uint` | Two concurrent streams per association. |

The active profile permits 32 KiB total receive credit, 16 KiB per stream,
6 reliable-history ranges, 12 transmission records and 32 KiB retained receive
data. Limits are ceilings; buffers grow with traffic and are released with the
stream/association. Packets use the shared runtime pool, with a default ceiling
of 64 lazy slots and four idle buffers. The dynamic delivery FIFO starts with
32 descriptors and grows according to available heap. There are no separate
QUIC handle quotas or TX completion tables. The packet MTU remains 1,100 bytes.

## 2. `reset` — discard endpoint associations

No arguments. Returns an empty map after detaching the endpoint, disposing its
QUIC state and reattaching. Peers must reconnect. Already accepted native TX is
allowed to complete; its buffers are reclaimed by C without calling the old
module. Stopping or updating the module has the same ownership guarantee.

## 3. `stats` — transport counters

Read live counters, or the last observed live association when recording was
explicitly enabled. These counters are per association, not lifetime totals.
Recording allocates one small snapshot, keeps no packet data and is off by
default. Enabling starts a fresh recording; disabling or stopping frees it.
A retained snapshot may omit the final close/control packet after retirement.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Optional: true starts/clears recording; false frees it; absent reads. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Snapshot recording enabled. |
| 2 | `active` | `bool` | Association currently live. |
| 3 | `received_packets` | `uint` | Valid received QUIC packets. |
| 4 | `sent_packets` | `uint` | Emitted QUIC packets. |
| 5 | `duplicate_packets` | `uint` | Received packet numbers already in ACK ranges. |
| 6 | `out_of_order_packets` | `uint` | Received ahead of a packet-number gap. |
| 7 | `inferred_missing_packets` | `uint` | Gaps inferred from later receive progress; not measured radio loss. |
| 8 | `retransmitted_packets` | `uint` | Repeated transmissions. |
| 9 | `loss_events` | `uint` | QUIC loss-detection episodes. |
| 10 | `submission_failures` | `uint` | Bearer-reported physical failures. |
| 11 | `received_stream_packets` | `uint` | Received packets carrying STREAM data. |
| 12 | `received_control_packets` | `uint` | Received control-only packets. |
| 13 | `sent_stream_packets` | `uint` | Sent STREAM packets, including retransmissions. |
| 14 | `sent_control_packets` | `uint` | Sent control packets. |
| 15 | `sampled` | `bool` | A live or retained association snapshot is available. |
