# `quic` API (13)

Native endpoint registration, obtained with `Host.get_component("quic")`.
Vtable version 2 retains its `attach(rx_method)` and `detach()` endpoint prefix
and appends owned relay mechanisms. Relay policy and CBOR
administration belong to `disc` (47.3..9). Relay callers check the extended table size; existing endpoint-only modules
remain compatible with the prefix. QUIC protocol
state belongs to the module. Native opaque relay routes keep priority;
unhandled local frames reach the one attached endpoint on the module worker.
RX preserves link, peer token, timestamp and signal. READY/STATE events use
the same callback method with `CALL_LINK_EVENT` and no packet.

`relay_create(route, idle_ms, reuse)` allocates an available opaque DCID and
returns it in `route`. It returns 1 when reusing an identical live route owned
by the caller, or 0 for a new route. `relay_set` atomically creates or replaces
an explicit route; replacement preserves its activity timestamp and timeout.
`relay_get` enumerates owned routes and `relay_remove` closes one.
`relay_stats` returns monotonic `last_used_us` and `idle_ms`; `relay_idle`
changes the timeout without refreshing activity. Zero disables expiry.
`relay_expire` reclaims expired owned routes. Packet lookup enforces expiry
immediately and refreshes last use on a matching packet, including refused
egress; inspection does not. The owner schedules cleanup and releases links.

`detach()` stops admission. The module closes its sessions and protocol holders.
Already accepted driver TX finishes independently and releases its own pool
reference; it never calls a stopped or replacement endpoint.

Packet services are generic Host calls: `pkt_pool_info`,
`pkt_acquire(reserve, out)` and `pkt_tx(packet, start, len, link, peer)`.
There is one shared pool, no per-module packet quota or completion ledger.
TX BUSY retains the exact reference for another path or retry. All other
outcomes consume it. Accepted TX is admission, not remote delivery evidence.

UDP peer tokens are opaque reply addresses scoped to the live listener. They
preserve IPv4/IPv6 address, scope and port in a bounded expiring peer table.
Closed-listener or recycled tokens are rejected; tokens are not identities.
