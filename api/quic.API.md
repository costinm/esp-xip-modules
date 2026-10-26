# `quic` API (13)

Native owned-packet endpoint services. Obtain the `Quic` vtable through
`Host.get_component("quic")`; methods take the calling module's host context.
This is a native ABI, not console methods. QUIC protocol state belongs to the
module. At most one module endpoint is attached. Existing native opaque relay
routes retain priority; unhandled local frames reach the attached endpoint.

`attach(rx_method)` enables endpoint delivery to the module worker, preserving
link, peer token, timestamp and signal. `detach()` stops delivery immediately.
No module function pointer is held by a native driver. Stop automatically detaches.
Attachment temporarily sets the endpoint's owned-packet quota to seven (one held
entry remains available for borrowed RX). Detach restores its previous quota;
it does not preallocate those buffers. Four physical TX completion records are
reserved while attached. The endpoint shares the ordinary native packet pool.

`pool_info(out)` reports physical geometry and the calling module's quota.
`acquire(reserved, out_packet)` atomically obtains one owned packet only when the
requested reserve remains. Module-held packet limits still apply.

`try_send(packet, start, len, link, peer, out_ticket)` transfers a prepared owned
packet only on success. **Every immediate negative result retains the exact
handle, range and backing bytes**, including BUSY. This deliberately differs
from consuming `Host.send`; its contract is unchanged. Accepted native TX may
outlive module stop. No module callback runs at native completion.

`poll_tx(out)` returns 1 for a completion, 0 when none is ready, or a negative
error. A completion returns the same physical packet with a fresh owned handle,
its ticket, range, link/peer, outcome and elapsed time. A full module inbox cannot
lose it. Polling can temporarily fail if the held-packet quota is full; release
another packet and retry. Detach discards pending completion records and reclaims
returned packets; C reclaims later driver returns without calling a new instance.

UDP peer tokens are opaque and scoped to the live UDP listener. They retain full
IPv4/IPv6 address, scope and port through a bounded expiring native peer table.
Tokens from a closed listener or a recycled peer record are rejected. They are
reply addresses, not identities or authorization.
