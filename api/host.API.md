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
This call never invokes another module. No sys module is needed. No partial record is returned on buffer overflow.

**Access:** authenticated

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `device` | `Device` | The device. |
| 2 | `wifi` | `WifiStatus` | The Wi-Fi status; an empty map when the host has none. |
| 3 | `heap` | `Heap` | As `heap_info`. |
| 4 | `modules` | `Slot[]` | Every module slot; an empty array when the host cannot enumerate them. |
| 5 | `components` | `map` | `{component: {method: {1: updated_ms, 2: status_map}}}`, milliseconds since boot. |
| 6 | `packets` | `Packets` | Global packet queue and pool snapshot below; empty when unsupported. |
| 7 | `power` | `PowerInfo` | Native sleep state, attempts, entries, skips, durations, wake cause and activity vetoes; independent of optional modules. |

`Device` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `uptime_ms` | `u64` | Milliseconds since boot. |
| 2 | `chip` | `u32` | An ESP chip model number. |
| 3 | `flash_size` | `u32` | Bytes. |
| 4 | `partition_label` | `string` | The running partition; omitted when the platform cannot identify it. |
| 5 | `partition_address` | `u32` | Its address. |
| 6 | `partition_size` | `u32` | Its size. |
| 7 | `mac` | `bytes` | Factory Wi-Fi station MAC, six bytes. |
| 8 | `image_address` | `u32` | Physical start of the executing runtime within its partition; omitted when unknown. |
| 9 | `image_size` | `u32` | Its 64 KiB-rounded flash span; omitted when unknown. |

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
| 5 | `version` | `u32` | Of the active image: its manifest version (key 8, 0 without one); higher is newer. |
| 6 | `last` | `i32` | What the module returned the last time. |
| 7 | `runs` | `u32` | Handler calls so far. |
| 8 | `ms` | `u32` | Duration of the last run. |
| 9 | `stack_free` | `u32` | Worker stack high-water mark. |
| 10 | `check` | `i32` | Result of the last start attempt's image check. |
| 11 | `reboot` | `bool` | The module stopped with memory it had not freed: it cannot be replaced until the device restarts. |
| 12 | `page` | `u32` | The 64 KiB page of the module partition the active image sits on. |
| 13 | `shadowed` | `Shadowed[]` | The other installed images of this module name: lower versions (kept for recovery, the first pages to be reused) and duplicates. Empty when none. |
| 14 | `quarantined` | `bool` | A Rust panic disabled this module until power-on or explicit quarantine clearing; start requests are rejected. |
| 15 | `heap_used` | `u32` | Live managed allocations in bytes; excludes native SDK allocations, runtime-owned context/inbox storage and stack. Zero without a live instance. |
| 16 | `heap_quota` | `u32` | Managed allocation limit in bytes; 4294967295 means unlimited. Zero without a live instance. |

`PowerInfo` fields:

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
| 28 | `usb_sleep_blocked` | `bool` | Attached/selected USB console currently vetoes physical sleep. |
| 29 | `usb_sleep_allowed` | `bool` | USB sleep opt-in policy is enabled. |
| 30 | `awake_remaining_ms` | `u32` | Remaining explicit temporary awake hold; this read does not extend it. |

The native panic discriminator is key 0 with value 3.

`Panic` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `event` | `string` | panic. |
| 2 | `module` | `string` | Module name. |
| 3 | `message` | `bytes` | Rust panic message. |
| 4 | `requested` | `u32` | Failed allocation size. |
| 5 | `free` | `u32` | Free internal heap at rejection. |
| 6 | `largest` | `u32` | Largest free internal block. |
| 7 | `used` | `u32` | Module managed allocations. |
| 8 | `quota` | `u32` | Module managed allocation limit. |
| 9 | `reason` | `u32` | 1 reserve, 2 allocator/quota, 3 post-allocation reserve. |
| 10 | `module_version` | `u32` | Crashing module version. |
| 11 | `internal_free` | `u32` | Free internal RAM at allocation failure. |
| 12 | `internal_largest` | `u32` | Largest free internal block at failure. |
| 13 | `pool_allocated` | `u32` | Packet slots holding storage at failure. |
| 14 | `pool_cached` | `u32` | Idle packet buffers available for reclamation. |
| 15 | `queue_depth` | `u32` | Queued packet references at failure. |
| 16 | `queue_capacity` | `u32` | Allocated queue descriptors at failure. |
| 17 | `module_heap_used` | `u32` | Sum of live modules' managed heap payload bytes. |
| 18 | `reclaimed_bytes` | `u32` | Idle packet storage reclaimed during this allocation attempt. |

`Shadowed` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `page` | `u32` | The page of the shadowed image. |
| 2 | `version` | `u32` | Its version (manifest key 8). |
| 3 | `duplicate` | `bool` | The same version as the active image (the lowest page is the active one) instead of a lower one. |


The native `identity` component is available to trusted modules through
`get_component("identity")`. Its Rust declaration is `xip_abi::Identity` and C
layout is generated in `include/xip.h`. It exposes the persistent device P-256
public key, message signing, and SEC1 signature verification. Private key bytes
never enter module or console responses. This is a native component, with no
remote console signing method; announcement wire format and caching policy
belong to its module caller.


`Packets` fields:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `depth` | `u32` | Pool references currently queued for modules. |
| 2 | `capacity` | `u32` | Allocated global queue descriptors; starts at 32, grows under memory budget. |
| 3 | `high_water` | `u32` | Maximum queued references since runtime initialization. |
| 4 | `accepted` | `u32` | Packet references accepted into the FIFO. |
| 5 | `dispatched` | `u32` | References handed to a live target for dispatch. |
| 6 | `queue_full` | `u32` | Refused because no queue descriptor was available. |
| 7 | `owner_stopped` | `u32` | Queued references cancelled when their instance ended. |
| 8 | `rx_pool_empty` | `u32` | Native receive refusals because no packet storage was available. |
| 9 | `pool_slots` | `u32` | Physical slot ceiling; native default 64, storage allocated lazily. |
| 10 | `pool_allocated` | `u32` | Physical slots with storage, including in-use and cached slots. |
| 11 | `pool_available` | `u32` | Unclaimed slots; some may not have storage allocated yet. |
| 12 | `counters_enabled` | `bool` | Counter collection is built in; otherwise tags 3–8 report zero. |
| 13 | `rx_ready` | `u32` | Idle allocated slots available to callback RX without allocation; zero if an external provider cannot report it. |

One FIFO holds references for all modules, with no per-module packet ceiling.
Queue metadata is allocated dynamically on the owner; payloads stay in the common
pool. Queued packets are released if their target instance stops. Memory pressure
or a full queue refuses new packets; no callback grows the queue or packet storage.
The owner briefly warms up to 32 RX slots after a burst/receive shortage and
returns to four cached slots after quiet operation. Counter collection can be
disabled with `CONFIG_XIP_PACKET_STATS`. These limits are independent of Wi-Fi's
SDK RX buffers and lwIP's UDP socket mailbox.

### Rust module panics

`get_component("module-panic")` returns the version-1 `xip_module_panic` table
(`ModulePanic` in the Rust ABI). The shared Rust panic handler calls `report`
with a bounded message and its current host context. The runtime emits a structured CBOR diagnostic,
records the module name in retained RTC RAM, and
restarts. The loader rejects every start of that name until a power cycle,
including explicit starts and requests that would start a component provider.
No module cleanup or flash write runs from the panic handler.

`read` returns a pending diagnostic as CBOR `{0:3, 1: "panic", 2: module_name,
3: message_bytes}` and the caller's module address. After reboot, mod_flash
appends this record through the files log API. It calls `logged` only after a
successful durable reply; errors retain the record for retry. Acknowledgement
marks the record logged without deleting the last diagnostic. Quarantine
remains after logging. RTC holds the last diagnostic and up to 32 disabled
names; a full list refuses all module starts. A power cycle clears RTC state.
Older runtimes without this component receive an ordinary reset request.

### Structured native fault diagnostics

Native console PPP port 1 carries CBOR fault records, rendered on the host by
`dmesh-cli console SERIAL watch`. No firmware printf line surrounds the record.
Panic record: `{0:3, 1:"panic", 2:module, 3:panic_message_bytes}`. When the same
module has an allocation failure recorded, fields 4–9 carry requested bytes,
free heap, largest free block, managed bytes used, quota and rejection reason.
Allocation denial record: `{0:4, 1:module, 2:bytes, 3:free, 4:largest,
5:used, 6:quota, 7:reason}`; reason 1 is host reserve, 2 is managed allocator
failure (heap/quota), 3 is post-allocation host reserve. These are diagnostics, not ABI
return codes. RTC holds diagnostics before reboot; mod_flash persists them
through the ordinary telemetry commit path after boot.

### Quarantine retry

`get_component("module-quarantine")` returns optional version-1 `ModuleQuarantine`.
Its `clear(user, name, len)` removes one exact name from RTC quarantine without
starting it or discarding pending diagnostics. Names contain 1–15 bytes without
NUL. Clearing an absent name succeeds. sys.quarantine_clear exposes this operation.

### Memory filesystem backend

`get_component("memory-files")` returns optional version-1 `MemoryFiles`.
The functions take explicit caller context. Names have 1–6 nonzero bytes and
namespace IDs are one byte: 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys (0 is flash,
served by mod_flash). open mode 0 pins a readable allocation, mode 1 allocates
an unpublished replacement, and mode 2 obtains a pinned stat snapshot, and mode 3 pins RAM/PSRAM for in-place
editing. An edit release always advances the version and does not support rollback. The
returned MemoryFile contains an address/length, version and lease token. Close
every successful open: commit publishes a complete replacement; abort preserves
the old contents. A read lease prevents replacement/removal. Addresses never
cross the remote API; native users must release leases before teardown.

Main reserves exactly 6144 retained bytes including panic state and RTC files.
ESP32 and S3 require RTC data in SLOW memory; C6 uses unified LP memory. Current
retained panic messages allow 768 bytes. Rust panic formatting borrows a packet
and falls back to 192 stack bytes if the pool is exhausted. No heap allocation
or filesystem write runs in the panic handler. mod_flash persists the diagnostic
after restart through the ordinary log API.

### Prepared panic file

`get_component("panic-flash")` returns optional version-1 `PanicFlash`.
During normal execution mod_flash prepares an erased non-cache named file and
arms its body address. The native service validates the `panic` file header,
partition bounds and erased contents. `range` pins that body against collection;
`disarm` is used before explicit removal. The panic path consumes the destination
once, skips a busy flash job, and writes the CBOR payload/header/commit with the
ordinary synchronous SDK driver while the Rust module worker still has an OS.
It performs no erase, allocation or filesystem calls. It does not handle CPU
exceptions or watchdogs. Pending RTC data remains available for reboot logging.
After `logged`, the pending read returns zero but proc/panic retains the last
message and module version until a new panic or power-on.


### Collector acknowledgement

Writing the exact archived proc/panic CBOR bytes to namespace 5 `panic` compares
with the retained record and clears it only on a match. A different newer panic
returns VERSION; an already empty diagnostic succeeds. Quarantine is unchanged.
The operation uses the existing file stream write API and does not require sys.
