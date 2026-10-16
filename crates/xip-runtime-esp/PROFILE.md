# Desired transport requests

`abi/API.md` owns the shared field tags and validation rules used by XIP and
DMesh firmware, Android and host. `tools/gen_abi.py` generates the C/Rust ABI
and field codecs. Requests are tagged CBOR, representing partial desired state.

Modules write through `transport_set(request, len, generation)`.
`transport_get` currently returns a coherent complete snapshot. Main callers
use `xip_runtime::profile` directly: `request` accepts CBOR, `update` accepts
its typed form. Both merge under the store lock; patches of different fields
do not overwrite one another. Identical requests retain the generation.
No decoder derives `run_requested` from mode. A field-only patch preserves
it; an explicit mode declaration follows DMesh's existing start/off semantics.

The mode rules are shared with DMesh: Off resets to the off baseline before
applying supplied fields; targeted STA clears the prior passphrase; target-less
STA preserves it and requires a configured target; NAN merges. Selecting mode
5 (UART) is unsupported for now. Independent UART fields remain valid, and
explicit uart_mode/uart_baud take precedence over the legacy selector in either
CBOR map order. Actual driver capabilities remain product-specific.

The Rust product request hook receives the mode, fields, wake_target and iface
while holding the store lock. It must not reenter the store or block. A lease
renewal or rejection cannot change state or generation. DMesh uses its existing
MAC admission and awake-lease policy in this hook. C supports the common mode
rules and rejects routing/wake metadata when no product processor is installed.
Owner notifications enqueue reconciliation; acceptance is not hardware completion.

Transport updates are ephemeral. Persistent boot settings continue through nv
and existing namespaces/keys (DMesh's dmesh/sta_ssid, sec/sta, uart.mode and
uart.baud). There is no profile blob or transport save/load API. Standalone
STA hosts use the same provisioned credentials at boot; DMesh also applies its
own NAN/sleepy policy. Quic, NAN/NOW scheduling and packet ownership stay with
DMesh's existing owners.
