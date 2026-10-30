# xip_ble: a thin NimBLE subset for the companion link

The runtime component `nimble`: it advertises the data a module gives it, scans with the module's filters, pairs, and opens one LE CoC channel to one
peer. The channel is a **link**: it carries QUIC packets, one per SDU, up to 1500 bytes. BLE is a compile option: with the profile `off` the component is
not in the image and `get_component("nimble")` returns NULL. The component holds **no product policy**; the `ble` component (104) that clients and
other modules use is hosted by the module `mod_ble`, which owns it: the service UUIDs, the advertisement and its intervals, the device name, the
default PSM, the scan filter and the SDU size and buffer count of the CoC pool are all passed to `start`, `scan` and `connect`.

## From a module

* **The ABI table.** `hw.nimble()` (Rust) or `host->get_component(host->user, "nimble")` (C), declared in `crates/xip-abi/src/nimble.rs` and `include/xip.h`
  (`xip_nimble`). The functions take `ctx` first, never block, and queue the work for the BLE task. Check for NULL (BLE off). Use it only from the
  module that owns BLE policy (`mod_ble`); everything else uses `get_component("ble")`, which `mod_ble` serves.
* **The methods.** `mod_ble` (component 104, no port, `modules/mod_ble/API.md`; requests are 0x444d0002 datagrams) serves the BLE operations as CBOR and, to other modules, as the `ble`
  table (thunks into the module): `start`, `stop`, `scan`, `scan_stop`, `scan_results`, `connect`, `listen`, `status`, `mode`, `bonds_clear`, `suspend`,
  `resume`. Tags and method numbers 80 to 85 are DMesh's.
* **The table functions.** `start(NimbleStart)` starts the stack when needed (the first call fixes the CoC pool: `coc_mtu`, `coc_buffers`), names the device,
  creates the CoC server on `psm` (0: none) and advertises `adv` / `scan_rsp` (`adv_len` 0: no advertisement, a server only: the `ble` method `listen`).
  `scan(NimbleScan)` takes the duration and up to three filters (an AD type, the element size and one element: the scan marks an advertisement that has it).
  `connect(NimbleConnect)` takes the peer, the PSM and the SDU size.
* **Start and stop.** Nothing runs until `start` (advertise what the module gives and serve the channel; the module's `listen` is a start without an advertisement),
  `scan` or `connect` (central: find a peer with `scan`/`scan_results`, then open the channel to it; the stack must have been started). `stop` ends advertising, scanning and the link.
* **Send and receive packets.** The channel is the runtime link `ble`: send a pool packet to it with the normal link send (a module's `send`,
  `console link.send` for debugging); a busy channel answers `ERR_BUSY` and keeps nothing of yours. Received SDUs are pool packets handed to the
  common QUIC receive function (`xip_quic_rx_set`, `xip_runtime.h`), not to modules.
* **Pairing.** The peer starts it; the component stores the bond. `status` tells `bonded` and whether the channel is `encrypted`. `bonds_clear`
  forgets the stored peers. One connection, one channel, one peer; two bonds are stored.
* **Status.** `status` reports ready, advertising, connected, channel open, counters of the channel (received, sent, refused), the last scan
  result, the local address, the mode and the boot flag.
* **Sleep modes.** `mode` 1 `always` (default): the service stays up and a light sleep is refused while it advertises, scans or is connected.
  `mode` 2 `rendezvous`: advertising and scanning stop for the sleep and restart after it; a live link still refuses the sleep.
  `suspend`/`resume` are for callers that manage sleep themselves.
* **Boot setting.** `ble.boot` (0/1) in the settings store; `mode` with `boot` 2 or 1 writes it. `mod_ble` starts the service about 5 s after the
  module starts when it is 1. With `XIP_BLE_RELEASE_WHEN_OFF` a boot with it 0 returns the controller's memory to the heap (BLE then needs a reboot
  with it set to 1).

## At a glance

| | |
|---|---|
| Profiles | Main defaults to `standard`; recovery always uses `off`. `XIP_BLE_PROFILE=` overrides Main for `scripts/build.sh` and `main-rs/build.sh`. |
| Compiled in, never started | about 50 KB less free heap and 30 KB more IRAM than `off`, about 390 KB of flash |
| Started, one peer connected | about 34 KB of heap more (classic ESP32, free heap 93 KB afterwards) |
| Kconfig | `XIP_BLE`, `XIP_BLE_COC` (`_MTU` and `_BUFS` are upper limits; the module chooses at start), `XIP_BLE_SCAN_CACHE`, `XIP_BLE_LOG_LEVEL`, `XIP_BLE_RELEASE_WHEN_OFF` |
