# xip_runtime

The ESP-IDF C implementation of the module ABI (`include/xip.h`): one worker task runs the handlers of every module, with
alarms, interrupts, requests, packets, the pool, settings, and the host calls and components of the table. Rust has a
separate implementation of the same ABI in `crates/xip-runtime-esp`; the host-runtime (`crates/xip-runtime-host`) is the
third.

**Wi-Fi.** `xip_wifi.c` is an optional simple owner of the radio (`xip_wifi_install_hooks`): station, soft AP or both, and
the scan. The wanted state is the `wifi` component, kept by `xip_wifi_intent.c`: a module (or the console, or `main`) calls
`set` with a whole `xip_wifi_intent`, the owner reconciles it on the worker, every module gets `HOST_WIFI`. The C runtime has
ESP-NOW (`xip_link_espnow.c`, the `espnow` link; read ESPNOW.md for why it is raw action frames) runs whenever the radio is on. Promiscuous receive for a NAN module is the `monitor` call of the `wifi` component (`xip_link_promisc.c`, the `promisc` link). The driver's buffer sizes (`wl` NVS namespace, or the `wifi-tuning` component) are applied when it starts.

The radio bring-up (`xip_wifi.c wifi_up`) and the ESP-NOW link are DMesh's tested sequence and action-frame code, ported; the Rust runtime
has the same in `crates/xip-runtime-esp/src/wifi_esp.rs` and `espnow_esp.rs`. The two are meant to behave identically, so any difference is a bug in one;
the frame codec (`xip_espnow_frame.h` and `espnow_frame.rs`) is checked against vectors generated from DMesh's code (`tests/data/espnow-frames.txt`).
Credentials a device keeps across restarts go through the `nv` host call (`sta_ssid`, `sec:sta`), which
`xip_wifi_intent_install` reads at boot.
