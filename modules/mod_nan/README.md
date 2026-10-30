# mod_nan

NAN service discovery and clock synchronization.

The runtime delivers radio frames through the module's virtual NAN port;
there is no firmware-specific radio setup in the module.

It follows the cluster clock on DW0/DW8 and sends NOW timing announcements on each wake, including
when following a peer.

Build with `scripts/build.sh esp32c6`; the package is `mod_nan.dmod`
(upload through the running mod_flash service). The installed module auto-starts;
query `nan.status` for synchronization. Use the five-second `discovery.nodes` stream for fresh discovery
observations; the module keeps no peer inventory. The module
caches an opaque supplied announcement, answers compatible discovery requests, and exchanges NAN
Follow-ups through the normal application dispatcher. `nan.announce` reads the
cached record; `nan.set_announce` replaces or clears it without restarting sync.
The companion or optional `mod_disc` supplies/signs that record; sync, sleep and
directed wake work even when no publication is configured. `nan.discover` requests active discovery; `nan.send` sends a
record to a peer. Start parameters
and outbound discovery/wake commands are described in [API.md](API.md).

With Wi-Fi connected, use the same channel for NAN and enable NOW before
starting the module. Start/stop preserves the infrastructure configuration.
For network replacement, start `flash` with the desired authentication mode
and use `dmesh-cli flash "$TARGET" IMAGE.dmod --unsigned --module nan --chip esp32c6`; the updater
waits for the old module to stop before writing. Start `nan` after a successful
upload and check its status. See [mod_flash](../mod_flash/README.md).

Use the `nan.sleepy` command to arm sleep between paired discovery windows.
The persisted `cadence` setting (missing/0/1: one DW, 8: about 4 seconds,
16: sixteen DW, 17: never sleep) arms sleep after 16 seconds uptime and five
seconds without activity. Local UART/USB bytes open grace immediately; network
packets count only after decryption, valid CBOR decoding and local direction
selection. Broadcast discovery never holds it awake. USB-JTAG attachment vetoes
sleep by default. Experimental `usb.sleep=1` detaches before sleep and keeps the
PHY detached across DW wakes until NAN transport.set explicitly selects serial
on again. Optional reports show sleep and Wi-Fi restart. See [the API](API.md#6-sleepy--arm-or-leave-coordinated-sleep).

Native commands can be invoked directly, such as `dmesh-cli "$PORT" nan.status`.
Configured LoRa/FSK roles use the native `lora` table: sleepy entry synchronously
quiesces radio resources, and activation restores the role. The module stays
mapped during suspension; explicit radio off unloads it.
The CLI wakes the UART companion and waits for an awake ACK first; `sleep`
explicitly ignores activity grace and uses saved cadence. `watch` is passive:
it displays compact DW0 summaries and live PUB/SUB/Follow-up observations.
Reports prefer an active BLE CoC and otherwise use UART, without an output
backlog or extra activity grace.

`transport.set` reaches NAN directly through manifest component ownership,
without the debug console. STA/AP and the other transport settings are
independent patches; peer wake remains `nan.wake`.

For live report bodies on a runtime stream, currently provided by QUIC:

```sh
dmesh-cli nan-stream DEVICE --seconds 30
dmesh-cli nan-stream udp://DEVICE_IP:3339 --seconds 30
```

Slow readers lose observations; use `--seconds` to bound the live report session.
