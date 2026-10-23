# xip_runtime

The ESP-IDF C implementation of the module ABI (`include/xip.h`): one worker task runs the handlers of every module, with
alarms, interrupts, requests, packets, the pool, settings, and the host calls and components of the table. Rust has a
bindings to these C services in `crates/xip-runtime-esp`; the native host implementation is
`crates/xip-runtime-host`.

**Wi-Fi.** `xip_wifi.c` is an optional simple owner of the radio (`xip_wifi_install_hooks`): station, soft AP or both, and
the scan. The wanted state is the `wifi` component, kept by `xip_wifi_intent.c`: a module (or the console, or `main`) calls
`set` with a whole `xip_wifi_intent`, the owner reconciles it on the worker, every module gets `HOST_WIFI`. The C runtime has
ESP-NOW (`xip_link_espnow.c`, the `espnow` link; read ESPNOW.md for why it is raw action frames) runs when the intent enables `now`. Promiscuous receive for a NAN module is the `monitor` call of the `wifi` component (`xip_link_promisc.c`, the `promisc` link). The driver's buffer sizes (`wl` NVS namespace, or the `wifi-tuning` component) are applied when it starts.

The generic C and Rust applications share the native radio owner (`xip_wifi.c`) and
ESP-NOW driver (`xip_link_espnow.c`), intended-state store and sleep hooks. Rust
startup installs the C owner unless the application supplies its own. DMesh's
`fw/transport` keeps its product policy. The single codec (inside `xip_link_espnow.c`) is
checked against raw-wire vectors shared with DMesh peers by the maintained Rust native-driver
test in `crates/xip-runtime-esp/tests/espnow_native.rs`, without root `tests/`.
See [ESPNOW.md](ESPNOW.md) for pooled ownership, pool admission, LR tuning and AP-free RX work.
Credentials a device keeps across restarts go through the `nv` host call (`sta_ssid`, `sec:sta`), which
`xip_wifi_intent_install` reads at boot.

**Socket readiness.** `xip_lwip_io.c` is the shared lwIP poller for module TCP
sockets and native UDP/TCP callback registrations. Firmware QUIC keeps its raw
UDP6 bearer; the optional lwIP UDP and TCP comparison backends have been removed. It has one lazy 6 KiB task and a
12-entry registry, with generation-checked tokens, suspend/rearm, periodic ticks
(including events=0), and deferred IPv6 TCP listener binding. The registry and
task are reclaimed when the final registration is cleared. Callbacks run outside
the registry lock and only drain bounded batches or wake an owner; module handlers
run on the host worker. Native descriptors remain adapter-owned. Deferred
listeners are service-owned; each accepted descriptor transfers to its callback.
Clear a watch before closing its descriptor; an in-flight callback may finish.
The unused Rust poller bindings and transport re-export have also been removed.

The maintained native Rust test compiles the actual C socket owner and poller
against SDK boundary fakes, plus real IPv6 UDP loopback sockets. It covers
mixed native/module registrations, listener startup before netif creation,
periodic work, queue pressure, stale handles, partial packet writes and teardown.

**Serial ownership.** `xip_serial.c` owns UART0/USB-JTAG driver setup, the receive
loop, independent optional transmit task, wake rearming and baud changes. A
product opens the same owner and supplies context-bearing codec/egress callbacks;
opening quiesces the early module-PPP reader first. The C owner releases client
state only after both callbacks have stopped, including task-creation failure.
DMesh supplies typed pool and QUIC consumer/completion callbacks in
`fw/transport/src/uart_esp.rs`; no second physical reader or Rust task stack
remains. Native tests cover handoff, duplex progress and teardown.

Application entry and Wi-Fi transition checkpoints print heap total/free without
allocating. Diagnostic `DMESH_FW_UART_ONLY=1` builds hold fw/main on module PPP
before networking; see the device memory test instructions in the CLI README.
The bounded heap-block snapshot captures descriptors under allocator locks and
outputs afterward. Console heap kind 6 streams CBOR records over native PPP;
its temporary descriptor allocation is reported and then freed.

The serial owner also handles PPP control ports `0xf8` (empty wake record) and
`0xf9` (request byte 0 + BE32 baud). C emits the matching ACK at the old rate,
drains TX, then changes UART0 to 115200/230400/460800/921600. Native USB-JTAG
rejects baud exchange. The control state is one scalar, with no packet pool
lease or private packet queue; module PPP writes and control ACKs serialize at
whole-record boundaries. FW and generic Main use the same C decoder and encoder. Rust supplies
only typed pool/consumer callbacks, with no serial codec or TX worker loop.

Every receive chunk extends a common activity window (default 4 seconds); a
wake record extends it to 60 seconds. Shorter activity never cuts a longer
wake interval short. `power.info.active_window` exposes the state to modules,
using a formerly reserved ABI byte. The native `xip_power_activate` and
`xip_power_active_remaining_ms` mechanism uses no timer task or heap allocation.
It does not acquire a PM lock or choose sleep policy. Rust FW delegates its
activity calls/queries to C and retains only product enable/always-on policy.

Persistent RX defaults are shared with C: 10 fixed buffers, 32 dynamic limit,
6-frame BA window. Use `console.nv` keys `wifi.rx_static`, `wifi.rx_dynamic`,
`wifi.rx_ba`; values are decimal ASCII bytes. They apply at driver initialization;
reboot after changes. Volatile nonzero `wifi.set_tuning` fields take precedence.

**Packet ownership.** `xip_serial_start_packets` binds a pool/consumer vtable
(Rust-defined in `crates/xip-abi/src/serial.rs`, generated into `include/xip.h`).
The shared C PPP decoder takes one exclusive pool writer at the port byte,
decodes directly into it, then commits or aborts exactly once. The application
chooses the pool and consumer; there is no full-frame scratch copy.

TX has **one current frame and no FIFO**. `xip_serial_submit_packet` calls its
synchronous ownership-move callback only after C admits the frame. Busy never
calls it, so the caller retains the exact original packet. C encodes from its
pool bytes using a transient 64-byte stack chunk, handles partial physical
writes, and calls completion once on success, error or teardown. Framing needs
no additional pool lease or per-packet heap allocation. Rust's remaining typed
completion callback returns the original submission to the QUIC owner; it
contains no framing, retry queue or physical-send decision. SDK byte rings/FIFOs
remain distinct from an application packet queue.
