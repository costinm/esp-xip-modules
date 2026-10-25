# `power` API (11)

Sleep and what the power code is doing, as the network-facing view of the `power` component (`xip_power` in `include/xip.h`, documented in
`crates/xip-abi/src/power.rs`). The runtime owns the sequence that makes light sleep work with the radio (DMesh's tested one); a module or the
console decides when. Each sleep stops the radio the way an epoch ends, arms the timer wake, light-sleeps, re-arms the UART receive, brings the
radio back with the same settings and delivers `HOST_WAKE` (an alarm with the window flag) to the modules.

## 1. `sleep` — Timer light sleep, a number of cycles

**C function:** power/Power.sleep

Returns at once; the sleeps run on the host worker with `awake_ms` awake between them so modules run. A rejected sleep (a power-management lock)
restores the radio once and drops the rest of the request. With a USB-Serial/JTAG host attached, the runtime uses a soft wait and stops/restarts Wi-Fi; physical sleep requires UART or disconnected USB.

**Execution:** non-blocking
**Access:** any of `system`, `mesh.owner`, `mesh.admin`

**Request:** [Sleep](#sleep)

### Errors

| Error | Meaning |
|---|---|
| `invalid_argument` | A duration under 10 ms. |
| `unavailable` | A request is already running. |

## 2. `info` — The power state and the FreeRTOS numbers

**C function:** power/Power.info

**Access:** authenticated

**Response:** [Info](#info)

## 3. `dump_locks` — Print the power-management locks to the device log

**C function:** power/Power.dump_locks

Who keeps the CPU frequency up or light sleep out (`esp_pm_dump_locks`).

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

## 4. `dump_tasks` — Print the FreeRTOS tasks to the device log

**C function:** power/Power.dump_tasks

Name, priority and stack left of every task.

**Access:** any of `system`, `mesh.owner`, `mesh.admin`

# Types

## `Sleep`

**C type:** PowerSleep

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `duration_ms` | `u32` | Length of each sleep, milliseconds (at least 10). |
| 2 | `cycles` | `u16` | How many sleeps; 0 is one. |
| 3 | `awake_ms` | `u16` | Awake time between two sleeps so modules run (0: 50). |

## `Info`

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
| 26 | `active_window` | `u8` | An activity interval opened by UART traffic or a wake record is still active. Modules decide sleep policy; querying this flag does not acquire a PM lock. |
| 27 | `ip_sockets` | `u8` | Open runtime TCP sockets plus the default UDP listener. Zero during NAN/NOW-only sleepy windows. |

## Native coordinated sleep and UART tracking

Power version 2 appends `sleep_until(ctx, wake_at_us, flags)`, `cancel(ctx)` and
`activate(ctx, duration_ms)`. These are native component methods for module
policy; they do not add remote power commands. `sleep_until` accepts an absolute
monotonic microsecond deadline and includes radio shutdown time in that budget.
An active UART window vetoes admission. New UART activity cancels pending
coordinated sleep or interrupts the USB soft wait. Physical UART wake re-arms
RX before restoring Wi-Fi. `cancel` affects only the calling module's request;
unload also cancels it. `activate(0)` clears the current activity window when an
explicit policy command arms sleep. New UART data opens it again.

`POWER_TRACE_UART` enables best effort CBOR records on reserved outgoing PPP
port **250 (0xfa)**, without a packet queue or persistent trace allocation:

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `phase` | `uint` | 0 before sleep, 1 after wake/radio resume, 2 rejected. |
| 2 | `time_us` | `uint` | Monotonic runtime timestamp. |
| 3 | `wake_cause` | `uint` | SDK cause: 4 timer, 8 UART; 0 if none. |
| 4 | `requested_us` | `uint` | Sleep budget at worker entry, including shutdown overhead for an absolute request. |
| 5 | `elapsed_us` | `uint` | Measured sleep/wait call duration; zero before entry. |
| 6 | `resume_us` | `uint` | Measured Wi-Fi/BLE restoration time; zero before entry. |
| 7 | `soft` | `bool` | USB soft wait rather than physical light sleep. |
| 8 | `physical_sleep` | `bool` | SDK light-sleep call succeeded on this boundary; false for soft waits and rejection. |
| 9 | `entries` | `uint` | Successful sleep/wait returns since boot, including soft waits. |
| 10 | `skipped` | `uint` | Rejected sleep attempts since boot. |
| 11 | `ip_sockets` | `uint` | Open IP sockets at this boundary; zero for sleepy NAN/NOW. |

A queued request or before-sleep record is not proof of sleep. Require an after
record with physical_sleep=true and an appropriate elapsed duration.
TODO: negotiate graceful USB shutdown/re-enumeration with the host, potentially
changing USB mode (including CDC Ethernet), before enabling physical sleep on
that path. The current soft fallback remains deliberate.

## Native temporary awake time

Power version 3 appends `awake_for(ctx, duration_ms)`. Any module may extend one
shared monotonic deadline by 1..300000 ms and renew later. Zero queries remaining
milliseconds; larger requests return ERR_ARG. It returns remaining milliseconds
and never shortens another caller's hold. It defers sleep at both admission and
radio-off boundaries without creating UART activity or changing sleepy policy.
