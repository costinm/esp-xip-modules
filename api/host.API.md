# `host` API (0)

The host's own status, as the CBOR, JSON and text view of its calls. The calls themselves, with their arguments and C
signatures, are the members of the host table (`xip_host` in `include/xip.h`, documented in `crates/xip-abi/src/host.rs`);
here are the maps they return. Method numbers are the slots of the host table. Wi-Fi is in `wifi.API.md`.

## 45. `heap_info` — Free memory of the device

RAM is the scarce resource on these chips, so the host reports it to any module that asks. "Largest block" is the biggest
single allocation that would succeed now; a heap can have plenty free and still refuse a request (fragmentation). Free
memory is tracked, not only reported: `lowest_free` shows the worst moment since boot, which is the number to size things by.

**Access:** authenticated

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `free` | `u32` | Free bytes. |
| 2 | `largest` | `u32` | The largest block an allocation can get now. |
| 3 | `lowest_free` | `u32` | The lowest free count since boot. |
| 4 | `total` | `u32` | Total bytes of the heap. |
| 5 | `free_internal` | `u32` | Free internal RAM. |
| 6 | `largest_internal` | `u32` | The largest internal block. |

## 60. `device_info` — Structured device and component status

Built-in records are read during the call; contributed records (a component's `info_publish`) are cached and may be stale.
This call never invokes another module. No console module is needed. No partial record is returned on buffer overflow.

**Access:** authenticated

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `device` | `Device` | The device. |
| 2 | `wifi` | `WifiStatus` | The Wi-Fi status; an empty map when the host has none. |
| 3 | `heap` | `Heap` | As `heap_info`. |
| 4 | `modules` | `Slot[]` | Every module slot; an empty array when the host cannot enumerate them. |
| 5 | `components` | `map` | `{component: {method: {1: updated_ms, 2: status_map}}}`, milliseconds since boot. |

`Device` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `uptime_ms` | `u64` | Milliseconds since boot. |
| 2 | `chip` | `u32` | An ESP chip model number. |
| 3 | `flash_size` | `u32` | Bytes. |
| 4 | `partition_label` | `string` | The running partition; omitted when the platform cannot identify it. |
| 5 | `partition_address` | `u32` | Its address. |
| 6 | `partition_size` | `u32` | Its size. |

`WifiStatus` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `sta` | `u8` | 0 off, 1 joining, 2 up (as `wifi.state`). |
| 2 | `ssid` | `string` | The joined network, when up. |
| 3 | `ip` | `string` | IPv4 address as text, when up. |
| 4 | `rssi` | `i8` | Signal in dBm, when up. |
| 5 | `ap_up` | `bool` | The soft AP is running. |
| 6 | `channel` | `u8` | The channel in use. |
| 7 | `now` | `NowStats` | The ESP-NOW action path; cumulative since boot (the C runtime; an empty map when the host has no radio). |

`NowStats` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `dispatcher` | `u32` | The private receive hook was called (either call form). |
| 2 | `tx_response_hook` | `u32` | The action request's in-band reply hook was called. |
| 3 | `actions` | `u32` | Delivered to the `espnow` link. |
| 4 | `self_echoes` | `u32` | Dropped: the source is one of our own addresses. |
| 5 | `duplicates` | `u32` | Dropped: a near-simultaneous second copy. |
| 6 | `invalid` | `u32` | Dropped: a malformed native call. |
| 7 | `parse` | `u32` | Dropped: the body does not parse. |
| 8 | `busy` | `u32` | Dropped: the other callback held the parser. |
| 9 | `drops` | `u32` | Every drop above. |
| 10 | `tx` | `u32` | Accepted by the driver. |
| 11 | `tx_failures` | `u32` | Refused by the driver. |
| 12 | `tx_last_error` | `i32` | The last driver error (ESP_ERR_*), 0 when clean. |
| 13 | `hook_calls` | `u32` | Every private receive-hook invocation, before any gate. |
| 14 | `disabled` | `u32` | The hook ran while the `espnow` link was stopped. |

`Heap` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `free` | `u32` | Free bytes. |
| 2 | `largest` | `u32` | The largest block an allocation can get now. |
| 3 | `lowest_free` | `u32` | The lowest free count since boot. |
| 4 | `total` | `u32` | Total bytes of the heap. |
| 5 | `free_internal` | `u32` | Free internal RAM. |
| 6 | `largest_internal` | `u32` | The largest internal block. |

`Slot` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `string` | The slot name. |
| 2 | `tag` | `u16` | The component number of the module. |
| 3 | `installed` | `bool` | A module is installed. |
| 4 | `running` | `bool` | The module is running. |
| 5 | `version` | `u32` | Of the installed package. |
| 6 | `last` | `i32` | What the module returned the last time. |
| 7 | `runs` | `u32` | Handler calls so far. |
| 8 | `ms` | `u32` | Duration of the last run. |
| 9 | `stack_free` | `u32` | Worker stack high-water mark. |
| 10 | `check` | `i32` | Result of the last start attempt's image check. |
| 11 | `reboot` | `bool` | The module stopped with memory it had not freed: it cannot be replaced until the device restarts. |
