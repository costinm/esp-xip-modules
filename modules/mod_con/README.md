# mod_con

Device control for module lifecycle, status, heap diagnostics, reset and saved
settings. Binary clients call `console.status`, `start`, `stop`, `reset`, `heap`
and `nv`, or use `dispatch` for a correlated console/native request. Native
Wi-Fi state and nonblocking scan results are encoded here from the host Wi-Fi
table. See
[API.md](API.md) for fields and responses.

Build it with `scripts/build.sh esp32c6` from the repository root. Install it
with the other modules; its manifest requests startup at boot. Firmware must
restart to publish newly installed methods.

Use [dmesh-cli](../../crates/dmesh-cli/README.md) to access devices:
`dmesh-cli console SERIAL console.status` for native module PPP, or
`dmesh-cli DEVICE console.status` through FW's DMesh dispatcher. Generic C
and host applications can use the module request API directly. UART requests are 0x444d0002 long packets in PPP records of port 0xf7 (`dmesh-cli` does this); the runtime parses the envelope, delivers the method to this module as an ordinary call (it has no port) and wraps the answer. Native components that no module hosts (Wi-Fi 10, power 11, ip 12, ...) are served by this module from the native method table; the runtime routes every envelope for a component without a hosting module to component 20. A module that holds an envelope (a NAN peer's follow-up) sends it to `ADDR_ENVELOPE` and the runtime dispatches it the same way.

See [the protocol guide](../../notes/docs/protocol.md) for manual requests and
[development notes](../../notes/ai/2026-10-23-runtime-implementation.md) for
implementation details.
