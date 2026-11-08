# `wifi` API (10)

The radio of the device, as the network-facing (CBOR/JSON) view of the `wifi` and `wifi-tuning` components. The C
structs modules use are in `crates/xip-abi/src/wifi.rs` (`xip_wifi_intent`, `xip_wifi_state`, `xip_wifi_tuning`); this file
names and numbers the same fields for consoles, gateways and tools. The mapping is one to one: `get` and `set` carry a
`WifiIntent`, `state` a `WifiState` and `tuning` / `set_tuning` a `WifiTuning`, with names and passphrases as text instead
of byte arrays with lengths.

The model is an **intended state**: a caller says what the radio should do and the device makes it true (joins, retries,
follows a channel change). There is no start or stop. With the radio on, `sta`, `ap` and `now` compose the state
(ESPNOW.md "Radio states"): soft AP + ESP-NOW, station + ESP-NOW, both, and off (the radio stops and is woken only by
a NAN/NOW discovery window, for sync); with `now` absent, the plain station — the one on state where the modem power
save runs instead, on the BSS beacon/DTIM schedule. Every state with ESP-NOW keeps the modem awake (ESP-NOW cannot run
against a sleeping modem), so the soft AP is a small extra cost — its ~512 ms beacon, which doubles as the soft-NAN
timing anchor. NOW-only operation (`sta=0, ap=0, now=1`) also receives without AP:
the radio owner registers a connectionless receive-window vote independently of ordinary modem sleep.

## 1. `get` — The intended state

**C function:** wifi/Wifi.get

**Access:** authenticated

**Response:** [Intent](#intent)

## 2. `set` — State what the radio should do

**C function:** wifi/Wifi.set

Replaces the whole intended state: a field that is absent takes its default (`radio` 0, `sta` 0, `ap` 0, `now` 0 — no
ESP-NOW, `channel` 0, no names). To change only the channel, `get`, edit and `set`. Returns once the state is accepted, not when it is true; read
`state` for that. An identical state changes nothing.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Intent](#intent)

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `generation` | `u32` | Increases when the intended state or the tuning changes. |

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A name or passphrase of the wrong length, a flag above 1, a channel above 14, or `sta` with neither `sta_ssid` nor `bssid`. |

## 3. `state` — What is true now

The role and radio flags remain present while off; local MACs are included when known. Joined SSID, IP and RSSI are omitted until associated. The sys component in `mod_system` encodes this snapshot from the host Wi-Fi table; it never starts another Wi-Fi owner.

**Access:** authenticated

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `sta` | [`StaState`](#enum-stastate) | The station. |
| 2 | `ssid` | `string` | The joined network; present when `sta` is `up`. |
| 3 | `ip` | `string` | IPv4 address of the station as text; present when `sta` is `up`. |
| 4 | `rssi` | `i8` | Signal of the joined network in dBm; present when `sta` is `up`. |
| 5 | `ap_up` | `bool` | The soft AP is running. |
| 6 | `channel` | `u8` | The channel in use; present when the radio is on. |
| 8 | `sta_mac` | `bytes` | Actual local station MAC, six bytes. |
| 9 | `ap_mac` | `bytes` | Actual local AP MAC, six bytes. |
| 10 | `radio` | `bool` | Driver is running independently of association. |
| 11 | `monitoring` | `bool` | Actual promiscuous receive state. |
| 12 | `now` | `bool` | Native NOW enabled for this epoch. |

## 4. `scan` — The networks in range

Never blocks: a scan takes seconds and a handler has a budget of milliseconds. The first call starts the scan and returns
`unavailable` with `retry`; poll about once a second and the call that finds it done returns the answer, once. It works in
every radio mode (station, ESP-NOW only, soft AP, joined); with the radio off it is switched on for the scan and off again.
A scan leaves the channel for about two seconds, so ESP-NOW and a soft AP do not hear their channel meanwhile; the radio is put
back on its channel afterwards. While the driver is busy joining the call returns `unavailable`. A network in range that is
not listed was missed (a scan can); scan again. A module gets the same list from the `scan` function of the `wifi` component.

The sys component in `mod_system` calls the host scan table and encodes the result. `ERR_BUSY` (-5) means retry; no private scan task or result cache is added.

**Execution:** non-blocking
**Access:** authenticated

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `networks` | `Network[]` | Strongest first. |

`Network` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ssid` | `bytes` | The name; not always text. |
| 2 | `bssid` | `bytes` | The access point, 6 bytes. |
| 3 | `rssi` | `i8` | Signal in dBm. |
| 4 | `channel` | `u8` | Primary channel. |
| 5 | `auth` | [`Auth`](#enum-auth) | Security. |

## 7. `monitor` — Promiscuous receive, for NAN

**C function:** wifi/Wifi.monitor

Switches promiscuous (raw) receive on or off. NAN discovery-window frames are not delivered any other way (see
`components/xip_runtime/ESPNOW.md`). The radio keeps receiving as before; the frames the driver then hands up also arrive as
packets on the link `promisc` of the `link` component: the 802.11 frame from its MAC header (radio FCS removed), the transmitter
address as the source token, the RSSI as the signal. The runtime applies an early receive filter (frames not for this device, or not of our networks or NAN, are dropped before they are copied) and does not change power save: the module
decides when to switch it off again (tens of milliseconds in a discovery window) and what to do about power save. The radio
must be on (`unavailable` otherwise); a radio restart clears it.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Monitor](#monitor)

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A channel above 13. |
| `unavailable` | The radio is off, or this host has no promiscuous receive. |

## 5. `tuning` — The advanced driver settings

**C function:** wifi-tuning/WifiTuner.get

Rarely needed settings for measurements and unusual links. The defaults of the device are the supported baseline.

**Access:** authenticated

**Response:** [Tuning](#tuning)

## 6. `set_tuning` — Change the advanced driver settings

**C function:** wifi-tuning/WifiTuner.set

Replaces all of them; an absent field is the default. `ampdu` and the four buffer sizes are read when the driver starts, so
a change restarts the radio (the station rejoins); the other fields apply to the next association.

Buffer overrides here are volatile. Persistent boot defaults use `sys.nv`
keys `wifi.rx_static`, `wifi.rx_dynamic`, and `wifi.rx_ba`; see the sys API.
The application baseline is 10 fixed RX buffers, a 32-buffer dynamic limit and
a 6-frame RX BA window. Stored defaults are applied on driver initialization;
nonzero fields in this tuning request take precedence. Reboot after changing
persistent defaults to apply them reliably.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Tuning](#tuning)

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A value is out of range. |
| `unavailable` | A setting this device does not have and that is not left at the default. |

# Types

## `Intent`

**C type:** WifiIntent

What the radio should do.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `radio` | `bool` | The radio is on. |
| 2 | `sta` | `bool` | Join `sta_ssid`, or the access point `bssid`, while the radio is on. |
| 3 | `ap` | `bool` | Run the soft AP while the radio is on. NOW-only operation can receive without AP using the connectionless receive-window vote; keep this independent of station association and NOW. |
| 4 | `channel` | `u8` | 0 any, else 1..14. Applies to the AP and speeds up a join. |
| 5 | `bssid` | `bytes` | Join this access point only (6 bytes). Together with `channel` it avoids a scan. |
| 6 | `sta_ssid` | `string` | Network name, 1..32 bytes. |
| 7 | `sta_psk` | `string` | Passphrase, 8..63 bytes; absent for an open network. |
| 8 | `ap_ssid` | `string` | Name of the soft AP, 1..32 bytes; absent: the device's default. |
| 9 | `ap_psk` | `string` | Passphrase of the soft AP, 8..63 bytes; absent for an open AP. |
| 10 | `now` | `bool` | Run ESP-NOW while the radio is on (the `espnow` link). Absent (0): no ESP-NOW — joined, the modem power save runs instead of the modem being held awake. |

## `Monitor`

**C type:** WifiMonitor

Promiscuous receive.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `enable` | `bool` | On (true) or off. |
| 2 | `channel` | `u8` | 0 leave the channel, 1..13 pin the unassociated radio to it (HT20) first. |

## `Tuning`

**C type:** WifiTuning

Switches are `default`, `on` or `off`; numbers are 0 for the default.
The C runtime supports opt-in raw STA UDP6 with `raw_rx=on`; default/off uses sockets.
`driver_tx` and `raw_tx_rate` remain unsupported in C.

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ampdu` | [`Switch`](#enum-switch) | A-MPDU aggregation in both directions. Default: on. |
| 2 | `rates_11b` | [`Switch`](#enum-switch) | 802.11b rates; applied before the radio starts. Default: off (DMesh's tested baseline). |
| 3 | `raw_rx` | [`Switch`](#enum-switch) | Opt-in native STA Ethernet ingress for link-local IPv6 UDP on the default QUIC port, with native Ethernet replies. Other traffic stays with lwIP. |
| 4 | `driver_tx` | [`Switch`](#enum-switch) | Rust runtime: station egress through the driver's data path; `off` is raw 802.11 injection. |
| 5 | `bssid_check` | [`Switch`](#enum-switch) | Standard BSSID filtering of the station's receive lane. Default: off, so frames of other BSSIDs reach unassociated ESP-NOW receive. |
| 6 | `raw_tx_rate` | `u8` | Rust runtime: raw transmit rate (driver rate code); 0 the driver default. |
| 7 | `rx_static` | `u16` | Static receive buffers, 2..64. |
| 8 | `rx_dynamic` | `u16` | Dynamic receive buffers, 2..128. |
| 9 | `tx_dynamic` | `u16` | Dynamic transmit buffers, 4..128. |
| 10 | `rx_ba` | `u16` | Receive block-ack window, 2..64; the driver limits it by the receive buffers. |
| 11 | `lr` | [`Switch`](#enum-switch) | Enable Espressif long-range protocol alongside 11b/g/n on the active interfaces. Default: off. Restarts the radio; does not force every transmission to use an LR rate. |
| 12 | `power` | `u8` | Modem power save override: 0 derives it from the state (`none` with ESP-NOW or without a station, `min` for the plain station), 1 forces `none` (the modem awake) in every state, 2 requests `min` for the plain station. A state with ESP-NOW always forces `none` — ESP-NOW cannot run against a sleeping modem. Applies at the next association. |

## enum `StaState`

| Value | Name | Description |
|---:|---|---|
| 0 | `off` | Not running. |
| 1 | `joining` | Joining or rejoining. |
| 2 | `up` | Joined, with an address. |

## enum `Auth`

| Value | Name | Description |
|---:|---|---|
| 0 | `open` | No security. |
| 1 | `wep` | WEP. |
| 2 | `wpa` | WPA. |
| 3 | `wpa2` | WPA2. |
| 4 | `wpa_wpa2` | WPA/WPA2. |
| 5 | `enterprise` | Enterprise. |
| 6 | `wpa3` | WPA3. |

## enum `Switch`

| Value | Name | Description |
|---:|---|---|
| 0 | `default` | The device's own default. |
| 1 | `on` | Enabled. |
| 2 | `off` | Disabled. |

## Native Wi-Fi version 2

WifiState appends actual STA/AP MACs and radio/monitor/NOW flags. The original
52-byte prefix remains readable on the C runtime. `sta=up` now means associated;
`ip4` is zero until IPv4 is ready.

`auto_reconnect(ctx, enabled)` returns the previous boolean and lets a module
own retry policy. `scan_channels(ctx, out, max, bitmap, dwell_ms)` is a bounded
non-blocking scan, with the normal scan completion contract. Bitmap bits 1..14
select channels; dwell is 10..120 ms per selected channel. Modules restore the
previous native reconnect policy on stop. These mechanisms do not prescribe a
particular AP, retry cadence or NAN policy.
