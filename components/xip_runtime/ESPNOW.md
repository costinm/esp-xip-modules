# Why ESP-NOW here is raw action frames with the Espressif OUI

This is the most expensive finding of DMesh's radio work. It is written down so it is not lost and not "simplified" later.

**On an ESP32 that is not connected, or that is connected but hears nodes other than its AP, the only kind of frame the device delivers on RX
(without promiscuous mode) is the raw 802.11 vendor-specific *action* frame of ESP-NOW: management subtype Action (0xD0), category 127
(0x7F), Espressif OUI `18:fe:34`.** Many other ways to receive something from a peer were tried over a long time, and none of them delivered.

What the DMesh sources say, which this note relies on (the full experiment log is DMesh's history; add to this list when it is rediscovered):

- `dmesh/crates/rawnan/README.md`: "ESP-NOW is used because on ESP32 the firmware allows receiving ESP-NOW action frames without requiring the more
  expensive promiscuous mode. Unfortunately no other action frame is allowed, except those in the same SSID/AP."
- `dmesh/fw/dmesh-fw-transport/src/wifi_esp.rs` (init): an unassociated STA admits continuous unsolicited vendor action `(127, 0)` while it does not
  deliver NAN / Public Vendor Specific `(4, 9)` through the private receive hook.
- `dmesh/crates/rawnan/src/p2p.rs`: the C6's private action receive hook does not deliver P2P public actions; only the passive beacon/probe IE is kept.
- NAN needs **promiscuous mode** (management filter), used for short windows only (a 64 ms capture in a 512 ms discovery window). It costs power, it
  cannot coexist with remain-on-channel, and on the classic ESP32 an associated station's promiscuous transition can suppress the raw Ethernet
  receive callback.

So the `espnow` link of the runtime is exactly that frame type, sent and received with the private action path rather than the `esp_now_*` API:

- receive: `ieee80211_recv_action_register(127, 0, cb)` (non-promiscuous; two call forms: the vendor-reassembled `(ctx, len, payload)` and the generic
  `(iface, header, body_start, body_end)`) plus the in-band reply hook (`rx_cb`) of `esp_wifi_action_tx_req`;
- transmit: `esp_wifi_action_tx_req`, wait 1000 ms, `no_ack` for broadcast (group-addressed frames are never acknowledged), BSSID ff:ff:ff:ff:ff:ff,
  interface STA (AP when only the soft AP is up);
- framing: category 0x7F, OUI 18:fe:34. **Only the first bytes need to be in the ESP format**: the 8-byte action header and one valid vendor element
  (up to 250 bytes of it); the rest of the datagram follows as raw bytes, ignoring the 802.11 element structure (nothing on the wire checks it). This is
  DMesh's `FirstIeThenTail`, which worked in the experiments; it was an option next to the older sequence of 250-byte elements, and the runtime keeps only this
  one for transmit. The parser still reads the older sequence, so frames from a peer that sends it are received. The codec is
  `xip_espnow_frame.h` / `espnow_frame.rs`, checked against vectors generated from DMesh's own (`tests/data/espnow-frames.txt`).
- the receive filter settings that go with it: the private libpp hooks `ic_rx_disable_bssid_check` / `ic_set_rx_policy_ubssid_check` (the
  `bssid_check` tuning, default off), the STA netif created before `esp_wifi_init`, power save off, HT20 on a pinned channel (default 6).

Consequences for the design:

1. Any node-to-node packet that must arrive while unassociated, or from a peer that is not the AP, is an ESP-NOW action frame. QUIC, the soft
   cluster's sync records and control traffic ride in it.
2. NAN (the Wi-Fi Aware service discovery of Android/iPhone) frames are *not* ESP-NOW and are not delivered like it: a NAN module needs the
   `monitor` call of the `wifi` component and the `promisc` link, for short windows only.
3. Do not replace this with `esp_now_*`, with data frames, or with probe/beacon vendor elements without re-running the receive experiments on the
   boards (unassociated, associated to another BSSID, AP+STA).
