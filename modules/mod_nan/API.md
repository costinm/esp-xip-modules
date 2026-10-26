# `nan` API (46)

NAN/NOW synchronization, discovery and radio policy on the C runtime. Virtual
port 246 (0xf6) receives NAN frames and NSY1 NOW bodies. QUIC stays on its normal
bearer/router. See [current behavior](../../notes/ai/nan/README.md) and
[troubleshooting](../../notes/ai/nan/TROUBLESHOOTING.md).

The installed module auto-starts and maintains synchronization. ASCII NVS
`cadence` missing/`0`/`1` selects one discovery-window cadence; `8` selects
about four seconds (8 DW), `16` selects 16 DW, and `17` means never sleep.
Sleep starts after 16 seconds uptime and at least five seconds since activity.
A DW cadence unit is 512 TU (524.288 ms); 8 is 4.194304 seconds. No device name
or power-meter board has special behavior.

By default, attached USB Serial/JTAG vetoes sleepy mode entirely, including
explicit `sleepy` requests. ASCII NV `usb.sleep=1` enables experimental USB PHY
detach before physical sleep (read at boot/reopen). It stays detached across
all timer/DW wakes. Only explicit serial-on selection by NAN `transport.set`
reattaches it; explicit serial on vetoes USB sleep until serial is selected off.
This does not repeatedly enumerate the host on each DW. Host recovery and power
savings still require device testing. USB is detected from SOF even without an
open terminal; power banks and external UART/USB bridges do not supply that
state to the ESP Serial/JTAG peripheral.

Local UART/USB bytes and the first physical UART wake byte immediately open a
five-second activity grace, allowing the full packet to arrive. Network input
votes only after decryption and full CBOR-envelope decoding in the runtime and
selection of a local directed record. Broadcast discovery, malformed/trailing
CBOR and nonlocal targets do not extend grace. Each accepted directed packet
renews it. NAN transport adapters use native `Power.received` after selecting a
local follow-up; ordinary runtime envelope delivery uses the same decoder.

The `nan` hosted table has native policy and supplier calls documented in
[API.local.md](API.local.md). The runtime calls USB/wake methods directly under
the module lock. Contextful discovery/watch requests retain their remote handler.
`transport.set` (component 1, method 4) routes directly to NAN through its
manifest's additional envelope ownership. Independent STA/AP/NOW, BLE, UART and
LoRa/FSK fields patch only the requested settings; there is no aggregate mode or
wake target. Peer activation remains nan.wake, whose legacy targeted radio
format is decoded only by the wake adapter. Unimplemented fields fail explicitly. Shared UART selector
18 is default/baud (0/1/6/7/9) or off (8); other rates are unsupported by the
current driver. Framing selector 28 accepts default/PPP (0/2). USB ignores baud;
serial-on restores a detached PHY and serial-off gates application UART RX while
preserving physical wake/recovery. BLE uses default/on/off (0/1/2). Radio mode is off/LoRa/FSK (0/1/2);
FSK retains its fixed 100 kbit/s profile. Radio frequency/bandwidth/SF/CR/power
settings require an explicit enabled radio mode. Transport admission is not
association, radio initialization or receiver-side delivery completion. Failures
are reported and may leave earlier independently applied transport changes.

NAN puts BLE in rendezvous mode and synchronously suspends configured LoRa/FSK
resources through its native table while sleepy; active transitions restore the
role. Suspension keeps the module mapped; explicit radio off unloads it. Native C
retains physical serial, BLE, SPI, pool and sleep mechanisms.

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
| 4 | `immediate` | `bool` | Default false; ignore the shared activity grace for this explicit sleep request. USB and other active-work vetoes still apply. |
| 3 | `cadence` | `uint` | Defaults to the saved NV cadence (one DW if automatic sleep is disabled); the wake period in 512-TU discovery windows: 1 (512 TU), 8 (DW0/DW8, 4096 TU) or 16 (8192 TU). Only read when enabling. |

Sleepy mode light-sleeps between the adjacent 16-TU NAN/NOW windows, which repeat
every `cadence × 512 TU` (default 1: every 512 TU; 8 is the DW0/DW8 four-second
cycle). Wi-Fi is NOW-only during timer wakes, with no IP listeners. Directed activity temporarily restores active roles; cadence resumes after its
five-second grace. A targeted NAN/NOW activation applies shared transport policy.
Temporary `awake` holds defer sleep without disabling the policy. Physical sleep
needs an unplugged/UART device by default; `usb.sleep=1` enables experimental
USB detach. A queued result does not prove physical sleep.

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
Renewal never shortens an existing hold. This is separate from packet/serial activity
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

## 12. `report` — current compact radio timing snapshot

**Response:** [Report](#report)

Operators may query the most recent receive-window timing and current clock state. This
request opens normal activity grace. Use the passive reports below when
measuring sleep; NAN retains timing state only, with no observation history.

On a runtime stream, the opening nan.report request registers one live report
sink. The runtime parses and dispatches that opening request. NAN writes a CBOR
sequence of Report bodies directly, without component/method/request-ID envelopes.
The first observation is the current snapshot, followed by live timing/discovery
reports. A slow reader loses observations; there is no event backlog or history.

The module uses only the transport-neutral Stream table and events. QUIC is the
current provider (including UART/BLE/UDP bearers); NAN creates no TCP listener
and contains no socket adapter. A future provider can use the same interface.
There are no commands or request bodies after the opening watch; extra input is
rejected without input buffering. Client FIN half-closes its sending direction
and reports continue. Full close or module stop releases the sink. Datagram
nan.watch and one-shot nan.report remain separate existing APIs.

## 13. `powerinfo` — The power state and the FreeRTOS numbers

**C function:** power/Power.info

**Access:** authenticated

**Response:** [PowerInfo](#powerinfo)

## 14. `powersleep` — Timer light sleep, a number of cycles

**C function:** power/Power.sleep

Returns at once; the sleeps run on the host worker with `awake_ms` awake between them so modules run. A rejected sleep (a power-management lock)
restores the radio once and drops the rest of the request. With a USB-Serial/JTAG host attached, the runtime uses a soft wait and stops/restarts Wi-Fi; physical sleep requires UART or disconnected USB.

**Execution:** non-blocking
**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [PowerSleep](#powersleep)

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A duration under 10 ms. |
| `unavailable` | A request is already running. |

## 15. `dump_locks` — Print the power-management locks to the device log

**C function:** power/Power.dump_locks

Who keeps the CPU frequency up or light sleep out (`esp_pm_dump_locks`).

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

# Events

## Live reports

NAN emits a compact report near the start of each DW0 receive window: one per
sixteen-DW cycle, approximately 8.39 seconds, independently of the configured
sleep cadence. Without a usable radio clock, the retry paths emit an
empty-cluster/no-timing report on the same interval using local uptime.
UART uses PPP port 251 (`0xfb`) with the CBOR map below. If a BLE
CoC is active it is preferred instead: the map is an unsolicited `nan.report`
result envelope `{1:46,2:12,6:report}` in a native `0x444d0002` datagram. There is
no request ID on these unsolicited reports. Android consumers should recognize
that form separately from correlated request responses.

# Types

## `Report`

The same schema covers periodic summaries and live observations; only relevant
fields are present. Kind 0 contains fields 2..5. Observation kinds 1..4 contain
fields 6..11. Observation records are opaque and untrusted.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `kind` | `uint` | 0 DW0 summary, 1 PUB, 2 SUB, 3 Follow-up, 4 NOW Service Info. |
| 2 | `cluster` | `bytes` | Last three bytes of cluster ID; empty if absent. |
| 3 | `first_rx_ms` | `uint` | Wake/capture-open to first NAN/NOW RX, saturated to 65535 ms; omitted if none. Includes radio resume time after physical wake. |
| 4 | `sync_ms` | `uint` | Wake/capture-open to first observed clock synchronization update; omitted if none. |
| 5 | `now_cluster` | `bool` | Local NOW anchor or a selected NOW clock parent is active. |
| 6 | `peer` | `bytes` | Source radio MAC, six bytes. |
| 7 | `record` | `bytes` | Opaque service information. |
| 8 | `rssi` | `int` | Signed received RSSI. |
| 9 | `service` | `bytes` | Six-byte NAN service ID; omitted for NOW. |
| 10 | `instance` | `uint` | NAN service instance; zero for NOW. |
| 11 | `via_now` | `bool` | True for NOW Service Info. |

These outputs do not create activity grace. Sending is best effort; full/busy
outputs lose an observation rather than retaining work. UART emission is not
yet gated on a host-watching indication. `dmesh-cli DEVICE watch` observes these
reports passively and does not send a wake request.


## `PowerSleep`

**C type:** PowerSleep

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `duration_ms` | `u32` | Length of each sleep, milliseconds (at least 10). |
| 2 | `cycles` | `u16` | How many sleeps; 0 is one. |
| 3 | `awake_ms` | `u16` | Awake time between two sleeps so modules run (0: 50). |

## `PowerInfo`

**C type:** PowerInfo

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `state` | `u8` | 0 idle, 1 sleeping, 2 resuming (the radio is coming back), 3 awake between cycles. |
| 2 | `pm_configured` | `u8` | ESP-IDF power management is configured (frequency scaling). |
| 3 | `auto_light_sleep` | `u8` | Automatic light sleep when idle (otherwise sleep is explicit). |
| 4 | `tickless_idle` | `u8` | FreeRTOS tickless idle is built in. |
| 5 | `cpu_mhz` | `u16` | CPU frequency now. |
| 6 | `min_mhz` | `u16` | Lowest frequency of the power configuration. |
| 7 | `max_mhz` | `u16` | Highest frequency of the power configuration. |
| 8 | `tick_hz` | `u16` | FreeRTOS tick rate. |
| 9 | `cycles_left` | `u16` | Sleeps still to run for the request. |
| 10 | `wake_cause` | `u8` | Why the last sleep ended (`esp_sleep_wakeup_cause_t`: 4 timer, 7 GPIO, 8 UART, 0 none). |
| 11 | `promiscuous` | `u8` | Promiscuous receive is on. |
| 12 | `sleep_attempts` | `u32` | Sleeps attempted. |
| 13 | `sleep_entries` | `u32` | Sleeps that returned without error. |
| 14 | `sleep_skipped` | `u32` | Sleeps rejected at once (a power-management lock). |
| 15 | `last_requested_us` | `u32` | The last sleep: microseconds asked. |
| 16 | `last_duration_us` | `u32` | The last sleep: microseconds it lasted. |
| 17 | `last_resume_us` | `u32` | The last sleep: microseconds from wake until the radio was started again. |
| 18 | `tasks` | `u32` | FreeRTOS tasks. |
| 19 | `ticks` | `u32` | FreeRTOS tick count since boot. |
| 20 | `heap_free` | `u32` | Internal heap free. |
| 21 | `heap_min` | `u32` | Internal heap, lowest ever. |
| 22 | `heap_largest` | `u32` | Internal heap, largest block. |
| 23 | `filter_dropped` | `u32` | Frames the early receive filter dropped. |
| 24 | `promisc_frames` | `u32` | Promiscuous frames delivered. |
| 25 | `soft_sleep` | `u8` | How the last sleep was done: 1 soft, 0 physical. Soft while the console is on USB-Serial/JTAG with a host attached (light sleep powers that port down and the host does not recover the device): the radio is stopped and restarted around a plain delay and the CPU does not sleep. Physical with a UART console or no host. |
| 26 | `active_window` | `u8` | A five-second activity interval from local serial bytes or a validated directed network record is still active. Modules decide sleep policy; querying this flag does not acquire a PM lock. |
| 27 | `ip_sockets` | `u8` | Open runtime TCP sockets plus the default UDP listener. Zero during NAN/NOW-only sleepy windows. |
