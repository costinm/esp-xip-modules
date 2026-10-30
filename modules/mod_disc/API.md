# `disc` API (47)

Discovery/identity policy.

The companion normally supplies signed records. 

Commands activate the module on demand. Lifecycle start parameters: optional CBOR `{1: channel_hint}`; default 6, range 1..13.
Start generates one cached signed local record. It does not start NAN or publish
automatically. Stop releases the cache and any bounded publication transaction.
Name/domain and actual Wi-Fi facts are sampled only on creation/explicit refresh.
The current record/identity format remains compatible with Android/shared codec;
certificate-chain policy and other discovery experiments are future extensions.

## 1. `announce` — read cached local record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `refresh` | `bool` | Default false; regenerate/sign before returning. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `record` | `bytes` | Cached signed discovery envelope. |

Refresh failure retains the old cache. Ordinary reads never sign. Changing this
cache alone does not replace an installed NAN publication.

## 2. `publish` — install cached record through NAN's public method

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `refresh` | `bool` | Default false; regenerate/sign first. |

Returns an empty map only after NAN acknowledges `set_announce` (46.11).
NAN is activated by the module request when needed. One transaction at a time; a second returns BUSY.
The two-second response deadline bounds transaction storage. Timeout returns
BUSY and leaves application completion unknown; query `nan.announce` to verify
whether the record was installed. A queued message alone is not success.
Stop cancels the wait, not a request already accepted by NAN. NAN retains its
own cached bytes independently if this supplier stops or is updated.

Publication requires the asynchronous native PPP/module-message path. A
synchronous application `xip_instance_request` (including FW's synchronous
module bridge) is rejected before queuing: it cannot wait for another module
from the shared worker. Such a caller can fetch `disc.announce` and invoke
`nan.set_announce` as two ordinary immediate requests instead. No runtime
request/worker ABI change is needed.

## 3. `relay_create` — allocate a forwarding with one destination

The runtime activates disc on the first request. No explicit start is needed.
No incoming DCID is supplied: disc returns an allocated opaque DCID. IDs are
checked against the native registry, allocated in a high range separate from
the endpoint's small sequential IDs, and never recycled within a runtime
lifetime. Stop/reboot invalidates routes.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `transport` | `string` | Link name, such as udp, espnow or ble. |
| 3 | `dest` | `any` | Link token, six-byte MAC, or compact 22-byte UDP address. CLI accepts MAC/UDP strings. |
| 4 | `rewritten_dcid` | `bytes` | DCID understood by the next hop. |
| 5 | `weight` | `uint` | Default 1; 1..65535 for creation. |
| 6 | `endpoint` | `bool` | Default false. True terminates an outer circuit; the destination is its reverse path. |
| 7 | `idle_ms` | `uint` | Default 60000; inactivity timeout, 0 disables expiry. |
| 8 | `reuse` | `bool` | Default true; reuse an identical live one-destination forwarding with the same endpoint/timeout policy. False allocates independently. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Allocated or reused incoming DCID. |
| 2 | `hops` | `RelayHop[]` | Current destinations, initially one. |
| 3 | `endpoint` | `bool` | Whether this binding terminates an outer circuit. |
| 4 | `idle_ms` | `uint` | Configured inactivity timeout. |
| 5 | `last_used_us` | `uint` | Last activity, device monotonic microseconds. |
| 6 | `idle_age_ms` | `uint` | Time since last activity at this read. |
| 7 | `reused` | `bool` | True when an identical live forwarding was reused. |

Activity is creation, explicit reuse, or a packet matching the route (including
an egress refusal). Reading, changing topology and changing timeout do not
refresh last use. Expired routes stop forwarding at their deadline. A one-second
module alarm reclaims their native storage and unused egress references; no idle
alarm is retained when no expiring routes remain. A timeout of 0 requires
explicit close/stop. Reuse is idempotence, not reference counting; choose
reuse=false for independently closed circuits.

## 4. `relay_add` — add a next hop or change its weight

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Existing incoming DCID. |
| 2 | `transport` | `string` | Egress link. |
| 3 | `dest` | `any` | Link token, MAC or UDP address. |
| 4 | `rewritten_dcid` | `bytes` | Next-hop DCID. |
| 5 | `weight` | `uint` | Default 1; 0..65535. Zero disables this alternative. |

The (transport, dest, rewritten_dcid) tuple identifies a next hop. An identical
tuple updates its weight; otherwise it is appended, up to four destinations.
At least one weight must remain positive. Changes are atomic and preserve the
route's DCID, timeout and last-use timestamp. Returns the current RelayRoute.

## 5. `relay_get` — read a forwarding

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |
| 2 | `hops` | `RelayHop[]` | Current destinations. |
| 3 | `endpoint` | `bool` | Endpoint mode. |
| 4 | `idle_ms` | `uint` | Inactivity timeout. |
| 5 | `last_used_us` | `uint` | Device monotonic microseconds. |
| 6 | `idle_age_ms` | `uint` | Current age. |

Expired or missing routes return NOTFOUND. Reads do not extend the lifetime.

## 6. `relay_list` — enumerate active forwardings

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `index` | `uint` | Default 0; sorted index for pagination. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `routes` | `RelayRoute[]` | Up to two active routes; empty ends enumeration. |
| 2 | `next` | `uint` | Next index; restart enumeration after edits or expiry. |

## 7. `relay_remove` — close a forwarding

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |

Returns an empty map after removal. Missing/expired routes return NOTFOUND.

## 8. `relay_remove_hop` — remove a next hop

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |
| 2 | `transport` | `string` | Egress link. |
| 3 | `dest` | `any` | Link token, MAC or UDP address. |
| 4 | `rewritten_dcid` | `bytes` | Next-hop DCID. |

Returns an empty map. If no enabled destinations remain, closes the forwarding.
A missing route or tuple returns NOTFOUND. Other removals preserve last use.

## 9. `relay_set_idle` — change inactivity policy

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |
| 7 | `idle_ms` | `uint` | New timeout; 0 disables expiry. |

Returns the current RelayRoute. This does not refresh last use. Reducing the
timeout below its current idle age closes the route on the next expiry check.

## 10. `discover` — experimental active route discovery

Enable `disc.routing` on each participant first. Sends a request immediately over
NOW, in the current/next NAN DW and in DW0/DW8. Nodes flood the first admitted
copy and retain a reverse path; only the target replies. No stream or master is used.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `target` | `bytes` | Required six-byte target STA MAC, different from origin. |
| 2 | `hop_limit` | `uint` | Default 4; 1..8. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `request_id` | `uint` | Origin sequence, correlated with candidate records. |
| 2 | `session` | `uint` | Random per-routing-activation session. |
| 3 | `timeout_ms` | `uint` | Origin receive hold and reply lifetime: 4000 + 18000 * hop_limit. |

Returns after NAN admits the solicitation. Admission is not transmission or
reachability. Poll `disc.routes` for a matching session/request. One local attempt
per minute; BUSY during an outstanding admission.

## 11. `routes` — experimental route candidates

Returns `{1: routes}` with at most eight RouteCandidate records. Expired records
are omitted. These unauthenticated observations do not install relays or prove
current bidirectional data delivery. Reads do not refresh lifetime.

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `routes` | `RouteCandidate[]` | Unexpired candidates, at most eight. |

## 12. `routing` — opt in or query active discovery participation

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Omit to query; true subscribes to NAN datagrams; false cancels work and frees routing state. |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enabled` | `bool` | Participation acknowledged by NAN. |
| 2 | `mac` | `bytes` | Local STA MAC used as the experiment node identifier. |
| 3 | `accepted` | `uint` | Distinct received requests admitted for four-second holds. |
| 4 | `duplicates` | `uint` | Request/reply duplicates suppressed across bearers. |
| 5 | `dropped` | `uint` | Ingress or downstream admission failures. |
| 6 | `last_error` | `int` | Most recent local/downstream error. |
| 7 | `routes` | `uint` | Unexpired candidate count. |
| 8 | `requests` | `uint` | Unexpired request count. |

Enable/disable waits up to two seconds for NAN. Stop disables the subscription,
cancels pending admission and frees routing state. An ambiguous timeout requires
querying NAN datagram status or retrying disable before assuming teardown completed.

Each accepted new request over NAN or NAN-over-NOW extends shared awake time by
4000 ms, without shortening an existing hold. Duplicates do not renew it. Admission
is bounded to four received requests per minute and eight retained request records;
full tables never evict live deduplication entries. Reverse requests and candidates
expire after 180 seconds; origin requests use timeout_ms. The first path is fixed
for each request, with hop count as cost and 0..31 ms jitter on forwarded broadcasts. Passive root trees, root search,
intermediate cached replies, route improvement and automatic forwarding are future work.

# Types

## `RelayHop`

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `transport` | `string` | Egress link. |
| 2 | `dest` | `any` | Token, six-byte MAC or 22-byte UDP address. |
| 3 | `rewritten_dcid` | `bytes` | Next-hop DCID. |
| 4 | `weight` | `uint` | Relative packet scheduling weight. |

## `RelayRoute`

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dcid` | `bytes` | Incoming DCID. |
| 2 | `hops` | `RelayHop[]` | Current destinations. |
| 3 | `endpoint` | `bool` | Endpoint mode. |
| 4 | `idle_ms` | `uint` | Inactivity timeout. |
| 5 | `last_used_us` | `uint` | Device monotonic microseconds. |
| 6 | `idle_age_ms` | `uint` | Current age. |

Dest UDP bytes are IPv6 (IPv4-mapped for IPv4), two-byte port, four-byte scope,
all network byte order. Configured addresses resolve against the current
listener on send; received UDP tokens remain scoped and expiring. Serial tokens
are PPP ports (247 for wrapped circuit traffic).

Alternatives use weighted round robin per packet. No health/failover, queue,
retry or remote delivery guarantee is implied. Install forward and reverse
routes explicitly. Endpoint bindings unwrap `0x40 DCID 0x00 opaque_packet` and
expose the complete packet on native link relay; their next hops are the reverse
path. The CLI selects only the first-hop DCID with --via FIRST_HOP:DCID.
Signed remote relay requests remain a future policy extension.

## `RouteCandidate`

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `target` | `bytes` | Target STA MAC. |
| 2 | `next_hop` | `bytes` | Immediate radio peer that supplied the reply. |
| 3 | `hops` | `uint` | Reply path hop count, 1..8. |
| 4 | `remaining_ms` | `uint` | Candidate lifetime remaining. |
| 5 | `channel` | `uint` | Receive channel. |
| 6 | `rssi` | `int` | Signed receive RSSI. |
| 7 | `now` | `bool` | Reply arrived over NOW rather than NAN. |
| 8 | `request_id` | `uint` | Origin request sequence. |
| 9 | `session` | `uint` | Origin routing-activation session. |

## Experiment radio contract

`src/api.rs` owns the bounded route-record codec. It is a CBOR map with tags
0=magic/version 0x485701, 1=kind (1 request, 2 reply), 2=origin STA MAC,
3=origin session, 4=origin sequence, 5=target STA MAC, 6=hops traversed,
7=hop limit and 8=reserved flags (zero). Requests and replies start with hops=0;
receivers add one. Invalid, duplicate-key, unsupported or trailing records are rejected.

The NAN-owned Rust contract is `esp_mod_nan::api`. Consumers select
`default-features = false, features = ["api-only"]` on the `esp-mod-nan`
dependency. Notification method -120 accepts `Received` records only from NAN
module messages, never remote envelopes.

Route-record consumers can likewise depend on `esp-mod-disc` with
`default-features = false, features = ["api-only"]` and use `esp_mod_disc::api`.
