# mod_nan

**Work in progress.** This will become the NAN / ESP-NOW discovery-window module: it will port the DW-related code of
`dmesh/fw` (`wifi_nan_dw_capture_esp.rs`, the DW8 sleep boundary of `main_runtime.rs`, the cluster and NOW-sync handling) onto the public components of
this runtime (`wifi`, `link`, `power`). It may depend on `dmesh/crates/rawnan` (the no_std NAN / ESP-NOW frame codecs, already shared with the
Linux and Android sides) instead of copying them. It starts small, with a test.

## Responsibility: coordinated sleep

This module owns **coordinated sleep**. That is the reason for NAN and all the dance around it: devices that sleep most of the time must still be able to talk to
each other, by meeting in the discovery windows (and the ESP-NOW windows next to them) and by telling each other when they sleep and wake. The runtime only
provides the mechanisms (radio stop/start with the same settings, timer light sleep, the early receive filter, promiscuous receive, the wake cause); *when* to sleep,
for how long, in which phase, and what to say about it is here. To port from DMesh (`fw/dmesh-fw-transport`):

- **Markers around a sleep.** DMesh announces `SLEEP_PENDING` before and `WAKE` after (`send_transition_announce`, `dmesh_server::announce`) on every live bearer: the NAN
  active publish, the ESP-NOW discovery broadcast, UDP when a station is up, and the line `DMESH sleep enter` / `DMESH sleep exit` (with the numbers) on the UART or JTAG, so a person or
  a host on the port sees the boundary. The bench already logs the two lines.
- **Packets while sleeping swap the device to active.** The UART wakes the CPU (`esp_sleep_enable_uart_wakeup`), a NAN follow-up / ESP-NOW control record arrives in the next window; a valid
  command can switch the profile to active (`control.transport.set { mode: ... }`) and every command opens an awake grace (`DEBUG_AWAKE_GRACE_MS`, 60 s) in which the device does not go back to sleep.
  The runtime tells the module why a sleep ended: the value of `HOST_WAKE` is the wake cause (4 timer, 8 UART, 7 GPIO) and the runtime drops the remaining cycles of a request woken early;
  the bench goes active again when that happens.
- **Wake a sleepy peer.** The directed NAN Subscribe (`ANNOUNCE_NAN_WAKEUP`, `queue_nan_wakeup`) that carries the `control.transport.set` record for a sleeping neighbour.
- **Phase.** Sleep until the next capture point of the selected cluster minus the measured wake lead (radio resume time + margin), never from a stale anchor; keep the exact capture chosen before
  the sleep after the resume; a rejected sleep gets a dwell, not a loop (`maybe_enter_sleep` in `main_runtime.rs`).
- **USB.** With a console on USB-Serial/JTAG and a host attached the runtime makes the sleep *soft* (the CPU stays awake; `power.info soft_sleep`): the C6 powers that port down in light sleep
  and the host does not recover the device (bench-tested, see the header of `components/xip_runtime/src/xip_power.c`). A device on USB is charged by the host anyway; when USB is disconnected the
  sleep is physical, and the sleep information (SLEEP_PENDING / WAKE, timing) travels in the DW announces, not on the port. So this module must work without USB and report through the announces.

## Today: a bench of the window

`src/lib.rs` runs one sequence and logs the result, to prove that the runtime parts a NAN module needs work together on a board:

```
disconnect (radio on, no station) -> sleep 500 ms -> promiscuous on, timer 20 ms -> count / print the frames
  -> promiscuous off, sleep 500 ms   (12 windows: a 500 ms sleep plus the resume drifts against the 512 TU window, so they sweep it)
  -> the wifi intent as it was -> look at the station again after 8 s
```

Start it with the console (`start nan`). Results go to the device log: sleeps entered and rejected,
the real sleep time, the radio resume time, frames seen (type, address 3, RSSI, length) and how many the early filter dropped, and the state of the station
afterwards. With a UART console the sleep is physical light sleep; with the console on USB-Serial/JTAG it is soft (`power.info soft_sleep`).

## What the runtime gives it

- `wifi.set`: radio on without a station (ESP-NOW only), and the station back afterwards; `wifi.monitor`: promiscuous receive on/off and the channel.
- `link`: the `promisc` link (frames from the driver after the early filter in IRAM: addressed to us or the group, network of ours or the NAN prefix) and `espnow`.
- `power.sleep`: the radio stopped as an epoch ends, timer light sleep, UART re-armed, the radio back with the same settings, `HOST_WAKE`.

Everything else (when to sleep, the cadence, the cluster anchor, what to announce, power save) is this module's policy.

## Build

Rust, like every new module: `XIP_RUST_MODULES=1 scripts/build.sh esp32c6` (needs the esp toolchain, `. ./env.sh`).
