# mod_con

Device control for module lifecycle, status, heap diagnostics, reset and saved
settings. Binary clients call `console.status`, `start`, `stop`, `reset`, `heap`
and `nv`, or use `dispatch` for a correlated console/native request. See
[API.md](API.md) for fields and responses.

Build it with `scripts/build.sh esp32c6` from the repository root. Install it
with the other modules; its manifest requests startup at boot. Firmware must
restart to publish newly installed methods.

Use [dmesh-cli](../../crates/dmesh-cli/README.md) to access devices. Generic C
and host applications can use the module request API directly. Legacy text
commands remain available for compatibility.

See [the protocol guide](../../notes/docs/protocol.md) for manual requests and
[development notes](../../notes/ai/2026-10-23-runtime-implementation.md) for
implementation details.
