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

`dmesh-cli discover` finds a companion-less device over its Wi-Fi connection.
Responses reuse the cached signed announcement.

A companion can skip this module and call `nan.set_announce` directly. No
certificate-chain protocol is implemented here yet. See [API.md](API.md) for commands.

## Active route experiment

Enable participation on each device, then request a target STA MAC from any one:

```sh
dmesh-cli console SERIAL disc.routing --enabled=true
dmesh-cli console SERIAL disc.discover --target=hex:001122334455 --hop_limit=4
dmesh-cli console SERIAL disc.routes
dmesh-cli console SERIAL disc.routing --enabled=false
```

Discovery uses NAN and NOW datagrams. It can wake sleeping participants for four
seconds and is limited to one local attempt per minute. Returned routes are
experimental candidates; this does not install forwarding. Use `nan.datagrams`
and `disc.routing` for progress. See [API.md](API.md) for timing and limits.
