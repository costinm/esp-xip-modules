# dmesh-fw-transport

ESP transport integration used by the DMesh firmware in `fw/main`. It provides
UART, Wi-Fi and ESP-NOW connectivity and connects them to the shared DMesh
services and QUIC library. Generic hardware/runtime services are supplied by
`components/xip_runtime`; portable DMesh libraries remain in the DMesh repository.

Build the consuming firmware from the repository root:

```sh
. ./env.sh
fw/build.sh esp32c6
```

To build without BLE:

```sh
DMESH_FW_NO_DEFAULT_FEATURES=1 DMESH_FW_FEATURES=modules \
  XIP_FW_TARGET_DIR="$PWD/target/fw-no-ble" fw/build.sh esp32c6
```

Use [dmesh-cli](../../crates/dmesh-cli/README.md) for discovery, configuration,
status and probes. Select transports through the published service methods;
persist boot settings through the settings/console API.

These adapters require the ESP toolchain and are built through the firmware,
not the host Cargo workspace. Implementation and migration details are in
[notes/](../../notes/ai/2026-10-23-runtime-implementation.md).
