# `nan` API (46)

NAN/NOW synchronization, discovery and radio policy on the C runtime. Virtual
port 246 (0xf6) receives NAN frames and NSY1 NOW bodies. QUIC stays on its normal
bearer/router. See [current behavior](../../notes/ai/nan/README.md) and
[troubleshooting](../../notes/ai/nan/TROUBLESHOOTING.md).

Start parameters: CBOR map `{1: channel, 2: service_info, 3: diagnostics,
4: miss_wait_ms, 5: lead_ms}`. Defaults: channel 6, empty publication, diagnostics false, miss wait 0, lead
8 ms. Service information is opaque and at most 255 bytes. The companion or a
separate supplier module owns its contents, signing and certificate validation.
NAN sync/sleep/wake work without a supplied announcement. Nondefault timing requires diagnostics; miss wait 0..512 ms,
lead 1..64 ms. Diagnostic allocations and all module state are freed on stop.

Associated STA never leaves its AP channel for NAN. Joining or off-channel STA
pauses NAN capture. Channel 6 continues NAN/NOW service for unassociated peers.
Native reconnect is disabled while module policy is active; one STA attempt is
bounded to ten seconds. Failed attempts fall back to NOW-only while retaining
STA intent. At minute intervals, a 40-ms/channel scan of 1/6/11 may start a new
attempt for the configured AP when its RSSI is at least -80 dBm. This is
opportunistic association, not a permanent AP inventory. Stop restores native
reconnect policy and the latest explicit configuration if still module-owned.

## 1. `status` — local synchronization and policy

### Response

Counter fields exist only when diagnostics is enabled. Peer inventories have
been removed; legacy fields 14 and 23 are empty. Use `watch` for fresh data.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `mac` | `bytes` | Current local radio source MAC. |
| 2 | `cluster` | `bytes` | Selected cluster, or six zero bytes. |
| 3 | `master` | `bool` | This device is a NOW anchor. |
| 4 | `tsf_us` | `uint` | Cluster reference TSF. |
| 5 | `received_us` | `uint` | Local reference timestamp. |
| 6 | `sync_rx` | `uint` | NOW sync observations. |
| 7 | `sdf_rx` | `uint` | Matching NAN service frames. |
| 8 | `beacons` | `uint` | NAN beacons. |
| 9 | `sync_tx` | `uint` | Accepted sync submissions, not delivery. |
| 10 | `sdf_tx` | `uint` | Accepted publications, not delivery. |
| 11 | `tx_errors` | `uint` | Transmission refusals. |
| 12 | `last_error` | `int` | Last native/module error. |
| 13 | `capturing` | `bool` | Actual capture state. |
| 14 | `peers` | `array` | Retired inventory; empty. |
| 15 | `selected_beacons` | `uint` | Selected-master beacons. |
| 16 | `max_beacon_gap_us` | `uint` | Largest selected-master gap. |
| 17 | `clock_master` | `bytes` | Selected transmitter or local source. |
| 18 | `clock_source` | `uint` | 0 none, 1 local, 2 NAN, 3 NOW. |
| 19 | `beacon_interval_tu` | `uint` | Selected advertised interval. |
| 20 | `diagnostics` | `bool` | Optional metrics enabled. |
| 21 | `signed_announce` | `bool` | Retired verification flag; always false. Consumers validate supplied signatures. |
| 22 | `announce_bytes` | `uint` | Local encoded length. |
| 23 | `identities` | `array` | Retired inventory; empty. |
| 24 | `sleepy` | `bool` | Sleep policy enabled. |
| 25 | `sleep_pending` | `bool` | Native sleep queued. |
| 26 | `resume_lead_us` | `uint` | Measured/conservative restart margin. |
| 27 | `ip_sockets` | `uint` | Native TCP/default UDP socket count. |

## 2. `tracking` — optional capture diagnostics

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Diagnostics enabled. |
| 2 | `opens` | `uint` | Capture openings. |
| 3 | `missed` | `uint` | Windows missing the selected beacon. |
| 4 | `recovered` | `uint` | Beacons found in the extension. |
| 5 | `timed_out` | `uint` | Extensions without a selected beacon. |
| 6 | `miss_wait_ms` | `uint` | Configured extension. |
| 7 | `samples` | `array` | Optional bounded capture samples. |
| 8 | `bytes` | `uint` | Optional diagnostic storage. |
| 9 | `lead_ms` | `uint` | Capture lead. |
| 10 | `device_capability` | `bytes` | Last selected NAN capability attribute. |
| 11 | `availability` | `bytes` | Last selected availability attribute. |
| 12 | `attributes_truncated` | `bool` | Attribute bound reached. |
| 13 | `attribute_source` | `bytes` | Attribute transmitter. |
| 14 | `attribute_tsf_us` | `uint` | Attribute observation TSF. |
| 15 | `last_response_id` | `uint` | Optional response correlation. |
| 16 | `last_response_peer` | `bytes` | Optional response source. |
| 17 | `last_response_now` | `bool` | Optional response transport. |

## 3. `announce` — cached local publication

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `record` | `bytes` | Current local encoded announcement. |

## 4. `discover` — queue an active NAN Subscribe

Returns `{1: request_id}` on queuing. This is one transmission intent, not a
collected inventory or proof of a peer response. Use `watch` to observe replies.

## 5. `send` — queue an application record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `peer` | `bytes` | Six-byte peer MAC. |
| 2 | `payload` | `bytes` | Opaque application record. |
| 3 | `instance` | `uint` | NAN peer service instance, default 1. |
| 4 | `now` | `bool` | NSY1 NOW instead of NAN Follow-up. |

Empty result means queued. NAN Follow-up payload is at most 231 bytes. BUSY
retains the exact prepared pool lease; stop/expiry releases it. Four bounded
control slots are available. Discovery observation events never use this queue.

## 6. `sleepy` — arm or leave coordinated sleep

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Required; enable or leave sleepy policy. |
| 2 | `trace` | `bool` | Default false; PPP sleep-boundary diagnostics. |
| 3 | `cadence` | `uint` | Default 1; the wake period in 512-TU discovery windows: 1 (512 TU), 8 (DW0/DW8, 4096 TU) or 16 (8192 TU). Only read when enabling. |

Sleepy mode light-sleeps between the adjacent 16-TU NAN/NOW windows, which repeat
every `cadence × 512 TU` (default 1: every 512 TU; 8 is the DW0/DW8 four-second
cycle). Wi-Fi is NOW-only during timer wakes, with no IP listeners. UART or
targeted NAN/NOW activation exits sleepy policy and restores active roles.
Temporary `awake` holds defer sleep without disabling the policy. Physical sleep
needs an external UART board; attached USB-JTAG uses the soft path.

## 7. `watch` — fresh observations for a bounded interval

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `duration_ms` | `uint` | Default 5000, maximum 300000; zero cancels the same route/request ID. |

An empty correlated response acknowledges registration. Subsequent CBOR
response datagrams use the same request ID and return route, including UDP or
NOW. Results are `{1:1, 2:peer_mac, 3:record, 4:received_us, 5:via_now, 6:rssi_dbm}`.
Expiration sends `{1:2}` as a best-effort completion. Renew/cancel by reusing
the route and request ID. Four registrations maximum; no registration allocation
when idle, no inventory, no replay and no event backlog. Raw observation data
is not a trusted identity assertion: consumers validate signed announcements.
A busy/full destination loses that event. Stop/update frees registrations.

## 8. `post` — arbitrary discovery SUB/PUB content

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `payload` | `bytes` | Opaque bounded content; NAN maximum 255 bytes. |
| 2 | `subscribe` | `bool` | Default false: PUB; true: SUB for NAN. |
| 3 | `now` | `bool` | Default false; true sends content as NSY1 Service Info. |

Queuing acknowledgement is `{1:0}`. No scan-result inventory is created. NOW
has no separate NAN SDA SUB/PUB flag; the supplied content defines its semantics.

## 9. `wake` — targeted activation through this device

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `target` | `bytes` | Remote unicast MAC, six bytes. |
| 2 | `kind` | `uint` | 1 STA (default), 6 NAN/AP. |
| 3 | `now` | `bool` | Default false NAN; true NSY1 NOW. |

Returns `{1: transaction_id}` on admission. One lazily allocated transaction
runs for at most twenty seconds. NAN requests discovery, then sends an Android
compatible type-2 Follow-up after observing the specified peer/instance. NOW
sends the same targeted CBOR directly. A correlated peer result/error ends the
transaction; otherwise it expires. Observe responses using `watch`: neither
admission nor a peer intent ACK establishes active IP/service readiness.
No pairing window or secret installation is opened by this operation.

## 10. `awake` — extend/query shared temporary awake time

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `duration_ms` | `uint` | Required, 0..300000; zero queries without changing the deadline. |

Returns `{1: remaining_ms}`. Other modules use native `Power.awake_for` directly.
Renewal never shortens an existing hold. This is separate from UART activity
and explicit active-mode transitions.

## 11. `set_announce` — replace cached publication without restarting

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `record` | `bytes` | Required opaque publication, 0..255 bytes; empty clears it. |

Returns an empty map after replacement. The companion or another module can
call this method; module calls use component 46/method 11. It is the same cache
as start field 2 and the `announce` getter. An absent/wrong-type or oversized
value fails before changing the existing record. No additional allocation,
signing, identity/certificate interpretation or persistence is performed.

Replacement preserves synchronization, sleep policy, watches and wake
transactions. Future publications and newly prepared discovery responses use
the replacement; already prepared packet leases remain unchanged until sent or
released. A supplied DMesh discovery envelope is answered by changing only its
outer correlation ID and copying the signed fields exactly. Other opaque
content is still published; it does not invent a DMesh identity response.
The supplier must budget the full response/wrapper against the 231-byte NAN
Follow-up limit if it wants directed discovery replies.

Trust/authorization and record freshness belong to the supplier/outer control
path. Received radio announcements remain live observations for consumers,
never requests to replace this cache. Stop/update discards the record; supply
it again at startup or with this method.
