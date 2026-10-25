# mod_nan

NAN service discovery and clock synchronization.

The runtime delivers radio frames through the module's virtual NAN port;
there is no firmware-specific radio setup in the module. It follows the cluster
clock on DW0/DW8 and sends NOW timing announcements on each wake, including
when following a peer.

Build with `scripts/build.sh esp32c6`; the package is `mod_nan.dmod`
(`scripts/flash-serial.sh SERIAL --module-only mod_nan`). Start runtime module
`nan` through console.start, then
query `nan.status` for synchronization. Use `nan.watch` for fresh discovery
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
and use `tools/flash_upload.py HOST module nan IMAGE --version N`; the uploader
waits for the old module to stop before writing. Start `nan` after a successful
upload and check its status. See [mod_flash](../mod_flash/README.md).

Android defines discovery/wake interoperability. For current behavior and
remaining migration work, see the [NAN/NOW overview](../../notes/ai/nan/README.md)
and [troubleshooting](../../notes/ai/nan/TROUBLESHOOTING.md).

Use the `nan.sleepy` command to arm sleep between paired discovery windows.
UART input or a targeted Android-compatible NAN/NOW wake request returns it to awake mode. Optional PPP boundary records report the
measured sleep and Wi-Fi restart; USB-JTAG uses a soft wait, so physical sleep
validation requires an external UART. See [the API](API.md#6-sleepy--arm-or-leave-coordinated-sleep).

The module reserves two consecutive link slots in `modules/SLOTS`; it remains
one independently upgradable image. Multi-page images require a current loader
and updater plus enough consecutive flash pages.
