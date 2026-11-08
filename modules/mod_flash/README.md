# mod_flash

Read and write files, telemetry batches, modules and runtime images over TCP 5227 or QUIC
streams. Authentication and authorization are supplied by the calling framework.
The module is autostarted at boot (its onBoot event) and joins saved Wi-Fi. It announces its recovery
service over UDP while normal discovery is unavailable; explicit recovery probes
still work when mod_disc is running.
Other modules use the direct `files` service (component 15) for small files and
telemetry: up to 1024 bytes per call, with range reads for larger files.
Image uploads can be retried with the same operation id after interruption.

See [API.md](API.md) for methods, fields and response framing. The host client
must use this stream API; the older `dmesh-cli flash` WLU1 uploader requires a
module built with the optional `legacy-upload` compatibility feature.

Build and test against a persistent 4 MiB flash file:

```sh
. ./env.sh
scripts/build-host-modules.sh flash
cargo build --manifest-path crates/Cargo.toml -p xip-runtime-host -p dmesh-cli
scripts/test-flash-standalone.sh
```

Create or inspect an offline flash image:

```sh
python3 tools/xipfs.py init target/flash-host.bin
python3 tools/xipfs.py list target/flash-host.bin
```

`init` creates a new image and refuses to overwrite an existing one. Close the
host runtime before changing its backing file with the offline utility.

Use the direct TCP stream client:

```sh
dmesh-cli tcp://DEVICE:5227 flash.fs.info
dmesh-cli tcp://DEVICE:5227 flash.file.write --name=config --input=config.cbor
dmesh-cli tcp://DEVICE:5227 flash.file.read --name=config --output=read.cbor
dmesh-cli flash-discover DEVICE:5227
dmesh-cli flash-discover /dev/serial/by-id/DEVICE
dmesh-cli flash-listen br-lan
```

Recovery needs only Main and this module. The primary update path is dmesh-cli
TCP 5227. With USB/serial, `dmesh-cli SERIAL flash.wifi.join --ssid=NETWORK
--psk=PASSPHRASE` waits for an IP using temporary credentials; nothing is saved.
Use `flash.wifi.info` to read the current network/IP. Append `--` and a
`flash.module.write` or `flash.runtime.write` command to upload over TCP while
continuing to show serial logs. See the [recovery commands](../../notes/ai/modules/flash-commands.md).
