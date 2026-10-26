# Runtime test module

`mod_rtest` provides packet ownership checks and a validated throughput probe.
Probe mode serves the same DMesh protocol over TCP 5228 and QUIC streams.
It verifies uploads, generates downloads and reports timing through `dmesh-cli`.

```sh
dmesh-cli "$PORT" console.start --name=test --request=hex:70726f6265
dmesh-cli "$PORT" console.start --name=quic
dmesh-cli probe "$PORT" --bytes=65536 --packet_size=512 --direction=both --verify=true
dmesh-cli probe udp://DEVICE_IP:3339 --bytes=65536 --direction=both --verify=true
dmesh-cli probe tcp://DEVICE_IP:5228 --bytes=65536 --direction=both --verify=true
dmesh-cli "$PORT" console.stop --name=test
```

If another test mode is running, stop it and wait for stopped status first.
Probe mode allows two connections; duplex uses one stream per direction.
Other test modes are described in [API.md](API.md). See the
[probe guide](../../notes/docs/probes.md) for NOW relays.
