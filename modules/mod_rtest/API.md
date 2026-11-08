# `test` API (2)

The device test module (`mod_rtest`, the `test` slot). Ordinary requests activate
the serving mode automatically; select diagnostic modes through `sys.start`.
Call its binary handlers with `dmesh-cli`. See the
[README](README.md) for the TCP/QUIC probe.


## 1. `echo` — answer with the same text and emit it as an event

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |

## 20. `pkt_echo` — send a packet back to its sender, unchanged but for the first byte

The packet itself goes back (no copy): `send(call.pkt, call.len, call.from, 2, 21, call.id)`. The answer is the call `21`.

## 22. `pkt_keep` — keep the packet past the call

Only for the tests of the packet rules: a kept packet that is still held when the module ends is counted as a leak.

## 23. `pkt_drop` — give the kept packet back

## 24. `mem_leak` — take memory and do not give it back

Only for the tests of the managed heap: the host frees the block when the module ends and reports the stop as failed (`ERR_LEAK`).

## 2. `info` — handler and NOW receive measurements

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `calls` | `uint` | Handler calls so far. |
| 2 | `mode` | `uint` | 0 selftest, 1 serve, 2 alarm, 3 gpio, 4 async, 5 NOW, 6 monitor. Probe mode uses a separate on-demand response described below. |
| 3 | `size` | `uint` | NOW only: requested payload bytes. |
| 4 | `sent` | `uint` | NOW only: submitted initiator packets. |
| 5 | `busy` | `uint` | NOW only: sender retained/freed a BUSY lease. |
| 6 | `tx_errors` | `uint` | NOW only: failed sends/completions. |
| 7 | `valid_ping` | `uint` | NOW only: received pings with full length and payload verified. |
| 8 | `valid_pong` | `uint` | NOW only: received pongs with full length and payload verified. |
| 9 | `echo` | `uint` | NOW only: submitted full-size responses. |
| 10 | `invalid_payload` | `uint` | NOW only: matching test packets with truncation or corruption. |
| 11 | `min_rx` | `uint` | NOW only: smallest verified received payload. |
| 12 | `max_rx` | `uint` | NOW only: largest verified received payload. |
| 13 | `tx_done` | `uint` | NOW only: completed initiator submissions; not receive proof. |
| 14 | `link` | `uint` | NOW only: opened link address. |
| 15 | `peer` | `uint` | NOW only: peer MAC token learned from a verified radio packet. |
| 16 | `destination` | `uint` | NOW only: configured MAC token; zero means broadcast. |


The NOW fields are absent in other modes. In NOW mode, port 3 accepts the same
numeric `info` envelope `{1:2,2:2,3:id,5:{}}` and returns a CBOR envelope with the
matching id and the result in field 6. The regular module method remains callable.

Start request: `now [paced|fast] [listen] [rxonly] [ap|sta] size=N [dest=TOKEN]
secs=N`. This is opaque module initialization data supplied through binary
`sys.start`, not a text command or a call to `parse()`.

`listen` disables periodic TX. `rxonly` validates received pings without sending
responses, so receive measurement does not depend on successful echo TX. The
pattern includes the declared length and verifies every byte; intact packets
from a different size row are ignored. `dest` is a decimal MAC token learned
from a prior verified packet. Without it, initiator traffic is broadcast, with
an occasional unicast probe after discovering a peer. Stop and the guard restore
the prior Wi-Fi intent. Payload room comes from the runtime's pool API; the
current pool permits 1,500-byte payloads; the link may impose a smaller limit. See `notes/ESPNOW.md` for the measured versus specified limits.

## Stream probe mode

A probe stream request activates `test` and its probe driver automatically.
Explicit `probe` start parameters remain available for diagnostics. It serves the
canonical DMesh `probe.handler` (component 8, method 2) over TCP 5228 and through
`mod_quic`. `probe.start` is the CLI's initiating operation; `mod_rtest` validates
uploads and generates downloads. Duplex uses two independent streams.

Payload size/packet size, direction and deterministic verification match DMesh.
Consumer delay fields are supported. Remote orchestration, priority streams and
ACK tuning are rejected. Two concurrent connections are admitted; all state is
allocated at activation and freed on stop. Other test activation modes retain
their existing behavior.

In probe mode, `test.info` returns field 1 as an array of live connections.
Each entry contains 1 handle, 2 QUIC flag, 3 request parsed, 4 input ended,
5 TCP write pending, 6 owned packet handle, 7 packet length and 8 output complete.
No connection history is retained.
