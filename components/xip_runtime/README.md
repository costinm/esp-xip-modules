# xip_runtime

ESP-IDF services for XIP modules: module execution, packet links, settings,
Wi-Fi, serial control, sockets and power operations. Use it from a C firmware
application or through [xip-runtime-esp](../../crates/xip-runtime-esp/README.md).

Use [C Main](../../main/main.c) as the integration example. Include this
component and `xip_loader` in the ESP-IDF build, initialize the host and loader,
then start the installed modules. Applications can use the supplied Wi-Fi owner
or provide their own radio integration.

From the repository root:

```sh
. ./env.sh
scripts/build.sh esp32c6
```

Use [dmesh-cli](../../crates/dmesh-cli/README.md) to control a device. Configure
saved network credentials and Wi-Fi tuning through the console API.

See [the module ABI](../../include/xip.h), [Wi-Fi](../../api/wifi.API.md),
[power](../../api/power.API.md), [console control](../../modules/mod_con/API.md)
and [ESP-NOW](ESPNOW.md) for the relevant contracts and options. Development
notes are under [notes/](../../notes/ai/2026-10-23-runtime-implementation.md).
