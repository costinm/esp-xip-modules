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

Experimental endpoint modules use the native [QUIC packet interface](../../api/quic.API.md).
The [raw-IP virtual interface](../../api/ip.API.md) lets a module connect complete
IP packets to native sockets. Both are optional; see [current behavior and
validation gaps](../../notes/ai/modules/README.md) before device experiments.

## Envelopes

A PPP record on a module's port, a datagram on its UDP port and a `0x444d0002`
QUIC datagram carry the CBOR envelope `{1: component, 2: method, 3: id,
5: fields}`. The runtime parses it ([xip_envelope.h](include/xip_envelope.h)); a
module never does. The module's handler receives an ordinary request with
`XIP_CALL_ENVELOPE`: the envelope's component and method, the fields map as
`data`, and the link and token to answer on. What it sends back with `reply` is
wrapped as `{1, 2, 3, 6: result}`; a bare negative integer (an `XIP_ERR_*`) becomes
`{1, 2, 3, 7: error}`, the one error shape. An answer may come later from another
call. Sending with `XIP_SEND_MORE` or-ed into the method keeps the request open
for more answers (a subscription's events); the last message omits it. A request
for a component the module does not serve is refused by the runtime with
`XIP_ERR_UNSUPPORTED` (the console module, component 20, takes every component and
also answers frames that are not envelopes with `XIP_ERR_ARG`). Bytes that are not
an envelope reach any other module as raw frames of the port (`XIP_CALL_LINK`).
