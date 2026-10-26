# mod_disc

Optional local discovery policy for devices operating without a companion, and
for discovery/certificate experiments. It generates a signed record on start,
caches it, and publishes through `nan.set_announce`. Native runtime services own
crypto and the private signing key. NAN has no dependency on this module.

Build with `scripts/build.sh esp32c6` (or the actual chip). Install the module,
then use this checkout's updated dmesh-cli:

```sh
dmesh-cli console SERIAL console.start --name=nan
dmesh-cli console SERIAL console.start --name=disc
dmesh-cli console SERIAL disc.publish --port 250
dmesh-cli console SERIAL nan.announce --port 246
```

An ordinary `disc.announce --port 250` reads the cache without signing.
Use `--refresh=true` on `disc.announce` or `disc.publish` only when the facts need
updating. Publication reports success after NAN's acknowledgement. It does not
prove radio delivery; test discovery from the receiver. Stop `disc` to free all
supplier state; NAN keeps its independent cached publication.

It also answers the runtime's UDP multicast discovery. The runtime's `xip_mc`
listener (on while there is an IP) joins the link-local group `ff02::5227` on
port 5227 and delivers each request to this module's port; the module replies
with the cached signed record correlated to the request's id and to the source
it came from. This is how `dmesh-cli discover` finds a companion-less device:
it needs no radio beyond the Wi-Fi link and reuses the signed cache, so a sweep
signs nothing.

A companion can skip this module and call `nan.set_announce` directly. No
certificate-chain protocol is implemented here yet; see [API.md](API.md), the
[NAN guide](../../notes/ai/nan/README.md) and
[module troubleshooting](../../notes/ai/modules/TROUBLESHOOTING.md).
