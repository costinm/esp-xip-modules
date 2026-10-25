# `ble` API (104)

Bluetooth LE companion link of the device, served by the module `mod_ble`, which hosts this component (manifest key 11) and owns its policy: the service
UUIDs, the advertisement and its intervals, the device name, the default PSM 0x0080, the scan filter and the size of the CoC pool. The mechanics are the
runtime's `nimble` component (`xip_nimble` in `include/xip.h`, `crates/xip-abi/src/nimble.rs`, implemented by `components/xip_ble`). Other modules reach
this API as a table too, `get_component("ble")` (`xip_ble` in `include/xip.h`, generated from this file by `tools/gen_vtables.py`). The module starts at boot and has no port of its own; direct requests are answered with the result map and an error as `{99: code}` (a negative `ERR_*` of the ABI); an envelope (a 0x444d0002 datagram, on PPP port 0xf7 for dmesh-cli) is answered as `{1, 2, 3, 6: result}` or `{.., 7: error}` by the runtime. The component number and methods 80 to 86 are the DMesh firmware's
(`dmesh-server` `ble` component 104: `start`, `stop`, `scan`, `status`, `scan_stop`, `connect`) with their field tags
unchanged; 87 and later are additions. The component advertises the DMesh service, scans, holds one LE CoC channel
(listen or connect; each SDU is one packet, so it serves as a bearer) and the bond store, and offers wake/sleep modes. What travels on the channel and
when to pair are decided by the caller.

Security defaults are fixed by the stack configuration and are not an API: bonding, LE secure connections, no MITM (no input/output
capability), security level 2, and the bond keys of both sides distributed. The CoC channel itself is plain (not encrypted) unless the peer
bonds; `status.encrypted` says which. Pairing is initiated by the peer; the component stores the bond and reports `bonded`.

A numeric envelope `{1: 104, 2: method, 3: id, 5: fields}` in a 0x444d0002 datagram reaches these methods: the runtime parses it and answers `{1, 2, 3, 6: result}` or
`{1, 2, 3, 7: error}` (the module only sees the method and the fields).
A byte field is bytes (the DMesh handler also takes text `hex:...`).

## 80. `start` — Start the service


Starts the BLE stack when needed, creates the CoC server on PSM 0x0080 and advertises the DMesh service (16-bit UUID 0x1820 and a 128-bit
UUID: the pairing UUID until the device holds a secret, the operational UUID after). The advertisement is restarted after every GAP disconnect
while the service is wanted. Returns once queued; read `status` for `advertising`.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Start](#start)

## 81. `stop` — Stop the service


Stops advertising and scanning and terminates the connection (reason 0x13).

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

## 82. `scan` — Scan for advertisements


Passive scan with duplicate filtering. Advertisements are counted in `status`; the newest distinct ones are kept in the scan cache (`scan_results`).

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Scan](#scan)

### Errors

| Error | Meaning |
|---|---|
| `timeout` | The stack is not started or not synchronized yet. |

## 83. `status` — The state


**Access:** authenticated

**Response:** [Status](#status)

## 84. `scan_stop` — Stop scanning


**Access:** any of `system`, `mesh.owner`, `mesh.admin`

## 85. `connect` — Connect and open the CoC channel


Cancels a scan in progress, connects as a central to `addr` (20 s timeout) and opens the CoC channel. Returns once queued; `status.coc_connected`
says when it is open.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Connect](#connect)

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A PSM outside 0x0080..0x00ff. |
| `timeout` | The stack is not started or not synchronized yet. |

## 87. `listen` — Create the CoC server only


Like `start` without the advertisement: the server for a peer that already knows the address.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Listen](#listen)

## 88. `bonds_clear` — Forget every bond


Deletes the stored peer keys: a bonded peer has to pair again and cannot reconnect with an old key. 

**Access:** any of `system`, `mesh.owner`

## 89. `mode` — Wake/sleep mode and boot flag


`mode` 1 (`always`): the service stays up across a sleep; while it is live a light sleep requested through the `power` component is
vetoed (advertising keeps the controller from sleeping on the current boards, so it is a power vote). `mode` 2 (`rendezvous`): advertising and
scanning are stopped before a sleep and re-armed after it, so the device is discoverable only in its awake windows; an established link still
vetoes the sleep (the transfer finishes, the link ends, the next sleep proceeds). `boot` 2 asks the application to start the service after the radio is up at
boot, 1 not to. A field at 0 is left unchanged. `boot` is also stored as the persistent setting `ble.boot`: the application reads it at boot (with `XIP_BLE_RELEASE_WHEN_OFF` a boot with the setting off gives the controller memory back, and BLE then needs a reboot with it on), and this module starts the service 5 s after its own start when it is on.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Mode](#mode)

## 90. `scan_results` — The scan cache


The distinct advertisements seen by scans since the cache was cleared (a new `scan` clears it), at most 16 and at most `CONFIG_XIP_BLE_SCAN_CACHE`.

**Access:** authenticated

**Response:** [ScanResults](#scanresults)

## 91. `suspend` — Prepare for a light sleep


What the `power` component runs before each sleep when the component is present, callable by a caller that owns its own sleep. Returns the
veto as an error (`unavailable`) when a sleep must not proceed.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

## 92. `resume` — Re-arm after a light sleep


**Access:** any of `system`, `mesh.owner`, `mesh.admin`

# Types

## `Start`

**C type:** BleStart

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `paired` | `bool` | The device holds a pairing secret: advertise the operational UUID. Absent: the pairing UUID (no pairing secret exists in the C runtime). |

## `Scan`

**C type:** BleScan

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `duration_ms` | `u32` | Milliseconds, clamped to 1000..30000; absent 10000. |

## `Connect`

**C type:** BleConnect

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `addr` | `bytes` | The peer, 6 bytes, as scan results report it. |
| 3 | `addr_type` | `u8` | 0 public, 1 random. |
| 4 | `psm` | `u16` | 0x0080..0x00ff; absent 0x0080. |

## `Listen`

**C type:** BleListen

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `psm` | `u16` | 0x0080..0x00ff; absent 0x0080. |

## `Mode`

**C type:** BleMode

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `mode` | `u8` | 0 unchanged, 1 always, 2 rendezvous. |
| 2 | `boot` | `u8` | 0 unchanged, 1 do not start at boot, 2 start at boot. |

## `Status`

**C type:** BleStatus

Tags 1 to 26 are the DMesh `ble.status`; 27 and later are additions. A zero is sent as a zero.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ready` | `bool` | The stack is synchronized. |
| 2 | `advertising` | `bool` | The service is wanted and advertised. |
| 3 | `connected` | `bool` | A GAP connection exists. |
| 4 | `coc_connected` | `bool` | The CoC channel is open. |
| 5 | `generation` | `u32` | Increments for every GAP connection. |
| 6 | `handle` | `u16` | GAP connection handle; 0xffff none. |
| 7 | `coc_rx` | `u32` | SDUs received on the channel. |
| 8 | `coc_rx_rejected` | `u32` | SDUs refused (bad length, no `ble` link). |
| 9 | `scanning` | `bool` | A scan is running. |
| 10 | `scan_reports` | `u32` | Advertisements seen while scanning. |
| 11 | `scan_matches` | `u32` | Those naming the DMesh service. |
| 12 | `last_scan_addr` | `bytes` | The last advertiser, 6 bytes. |
| 13 | `last_scan_addr_type` | `u8` | |
| 14 | `last_scan_rssi` | `i32` | dBm. |
| 15 | `bonded` | `bool` | A pairing completed since boot. |
| 16 | `coc_tx_pending` | `bool` | An SDU is queued and not yet submitted. |
| 17 | `local_addr` | `bytes` | The device's BLE address, 6 bytes. |
| 18 | `local_addr_type` | `u8` | |
| 19 | `ingress_enqueued` | `u32` | SDUs accepted for the consumer. |
| 20 | `ingress_accepted` | `u32` | Taken by the consumer. |
| 21 | `ingress_rejected` | `u32` | Refused by the consumer or oversize. |
| 22 | `egress_attempts` | `u32` | Transmit attempts. |
| 23 | `egress_sent` | `u32` | Accepted by the stack queue. |
| 24 | `egress_send_errors` | `u32` | Failed. |
| 25 | `egress_blocked_disconnected` | `u32` | Refused because no channel was open. |
| 26 | `egress_blocked_pending` | `u32` | Refused because the previous SDU was still queued. |
| 27 | `encrypted` | `bool` | The channel's connection is encrypted. |
| 28 | `mode` | `u8` | 1 always, 2 rendezvous. |
| 29 | `boot_enabled` | `bool` | BLE starts at boot. |
| 30 | `suspended` | `bool` | Stopped around a sleep (rendezvous mode). |

## `ScanResults`

**C type:** BleScanResults

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `results` | `bytes` | Records of 9 bytes: address (6), address type, RSSI (signed byte), flags (bit 0: names the DMesh service). |
