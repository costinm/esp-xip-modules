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
[NAN power methods](../../modules/mod_nan/API.md#13-powerinfo--the-power-state-and-the-freertos-numbers), [console control](../../modules/mod_con/API.md)
and [ESP-NOW](ESPNOW.md) for the relevant contracts and options.

Experimental endpoint modules use the native [QUIC packet interface](../../api/quic.API.md).
Native TCP listeners use the normal lwIP stack and module-declared stream ports.
Mesh relays and streams use QUIC. VIP/IP tunnel endpoints are not part of the runtime.

## Envelopes

The CBOR envelope `{1: component, 2: method, 3: id, 5: fields}` is parsed by the runtime in one place: a **QUIC long packet of version `0x444d0002`**
that the classifier takes from a QUIC link (PPP port `0xf7`, UDP 3339, `ble`, `espnow`). `dmesh-cli` sends every request that way. Module ports (PPP,
UDP, any) deliver **raw frames**, the full datagram or record, as `XIP_CALL_LINK`; nothing on them is parsed. The packet goes to the module of the envelope's
component if that module has a datagram port or hosts a component. An explicit additional envelope owner (manifest key 12) takes precedence;
`mod_nan` owns transport component 1 this way. Otherwise the request falls back to component 20 (`mod_con`), which serves still-unowned native
components (Wi-Fi 10, ip 12, ...) from its native method table. A primary module with neither a port nor a hosted component is not a target. The handler receives an ordinary
request with `XIP_CALL_ENVELOPE`: the envelope's component and method, the fields map as `data`, the link and token to answer on (`from`, `id`) and
the envelope's own id (`request_id`, `request_has_id`, appended to `xip_call`). What it sends back with `reply` is wrapped as `{1, 2, 3, 6: result}` and
sent to the peer as a `0x444d0002` packet with the CIDs swapped; a bare negative integer (an `XIP_ERR_*`) becomes `{1, 2, 3, 7: error}`, the one error
shape. An answer may come later from another call. Sending with `XIP_SEND_MORE` or-ed into the method keeps the request open for more answers (a
subscription's events, which keep the reply note of the datagram until it expires); the last message omits it. Fields that are not a map (the NOW bench's
pattern) make the packet a raw frame instead.

## Components hosted by a module

A component is a table of functions whether the runtime or a module implements it. A module lists the components it hosts in its manifest (key 11,
`mkmod.py --components ble`; at most three names). After the module started the runtime asks it for each table (`XIP_HOST_TABLE`, answered with
`Hw::provide_table`: the table `{u32 version, u32 size, one function pointer per method}` and the context its functions take).
`get_component(name)` falls through from the runtime's own tables to the running module that hosts the name and returns the runtime's **copy** of the
table, whose functions are thunks. A thunk takes the global module lock (the one that also serializes every handler), calls the module's function
with the module's context instead of the caller's, and returns `XIP_ERR_NOTFOUND` once no module hosts the name; a restarted module is served by the
same copy again. The functions are `int f(void *ctx, ...)` with at most five more words of arguments (integers and pointers; a 64-bit argument
takes two). The thunks run on the calling task, so any task may call, a module worker included: the call runs inline under the same recursive lock, so a hosted
function may call tables again (its own too) without deadlock. A runtime component of the same name wins. A module that is installed but not running is **started by `get_component`** (the `component_start` hook; the
loader starts the installed module that names the component, and its `HOST_START` and table exchange run inline before the answer); if it cannot start the
answer is NULL. When a module stops (upgrade, replacement, `module_rescan`) its table is gone with it: thunks handed out earlier return `XIP_ERR_NOTFOUND`
while it is stopped, and are bound again to the next instance of the name, which may be another image at other addresses (entries it lacks answer
`XIP_ERR_UNSUPPORTED`). The lock covers the swap: `finish` (which releases the image) takes it, so no thunk is inside module code while the image goes. The generator `tools/gen_vtables.py` writes the table, the CBOR glue and the module-side
wrappers from an `API.md` (`scripts/gen.sh`).

Which methods of a hosted vtable an envelope can reach: those of the module's `API.md` (numbered, CBOR, in the catalog) are remote-callable, and only
they are in the generated `dispatch`. Methods that only `get_component` callers use, with their own parameters (up to five words, like the psa vtable),
are declared in `API.local.md` beside it; they are in the table after the remote ones and no envelope reaches them.

## UART wake

Local RX bytes and a physical UART wake edge open five seconds of activity
without requiring a complete PPP packet. After restoring active module roles,
the serialized worker emits PPP port 248 (`0xf8`) with payload `01`: "I got
local input and am awake". Multiple RX chunks coalesce; a busy transmitter retains
the pending ACK. An empty record on port 248 is the host's idempotent wake probe.
The CLI sends a probe immediately, retries only that probe until ACK, then sends
the real request once on port 247 using the same port owner. It does not depend
on a DW announcement. Passive watch does not send probes. Native commands use
`dmesh-cli DEVICE METHOD`, with `wake`, `sleep`, and `nv KEY[=VALUE]` shortcuts;
the old `console` prefix is retained only for compatibility.
