# `transport` API (1)

Independent transport settings owned by NAN. Requests route directly to the NAN
module; the debug console does not participate. Omitted settings retain their
current values. Peer activation is a separate nan.wake command; the old aggregate
mode and wake_target fields are not part of this API.

## 4. `set` — select coordinated transport policy

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `sta` | `uint` | 0 off, 1 on; omitted retains STA. Enabling requires an existing station profile. |
| 14 | `nan_dw_interval` | `uint` | 0 default (8), 1, 8 or 16 discovery windows; omitted retains the interval. |
| 15 | `now` | `uint` | 0 default, 1 on, 2 off. Disabling the sync control plane is unsupported. |
| 16 | `ap` | `uint` | 0 off, 1 on. |
| 18 | `uart` | `uint` | 0/1 selects 115200, 6 selects 230400, 7 selects 460800, 8 off, 9 selects 921600. Serial on also restores a detached USB PHY; USB ignores baud. |
| 26 | `ble` | `uint` | 0 default, 1 start, 2 stop. Sleep policy uses rendezvous while sleeping. |
| 28 | `uart_mode` | `uint` | 0 default or 2 PPP. Other framing is unsupported. |
| 29 | `uart_baud` | `uint` | Explicit supported physical baud; overrides selector 18. USB ignores it. |
| 31 | `radio_mode` | `uint` | 0 off, 1 LoRa, 2 FSK. |
| 32 | `radio_frequency` | `uint` | Radio frequency in Hz for this activation. |
| 33 | `radio_bandwidth` | `uint` | Radio bandwidth in Hz. |
| 35 | `radio_tx_power` | `int` | Radio transmit power in dBm; negative values currently unsupported. |
| 36 | `radio_spreading_factor` | `uint` | LoRa spreading factor, 5..12; zero uses the default. |
| 37 | `radio_coding_rate` | `uint` | LoRa coding rate, 5..8; zero uses the default. |

Returns an empty result on admission. This is not receiver delivery, association
or radio initialization completion. Independent settings are not transactional;
an error may leave earlier settings applied. Radio parameter changes require an
explicit enabled radio mode. FSK uses the existing fixed 100 kbit/s profile.
