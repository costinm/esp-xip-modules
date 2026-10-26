# `stream` API (14)

Local transport-neutral stream delivery for modules. The fixed-layout table is
`Stream` in `crates/xip-abi/src/stream.rs`, obtained with `get_component("stream")`.
It has no remote CBOR methods.

Consumers may bind a component/method with `listen`; their primary component
also accepts streams when the manifest declares a stream port. Providers open
an opaque stream with `open(ctx, NULL, 0)` and feed its received bytes unchanged.
The runtime assembles the opening envelope (at most 512 bytes), resolves its
component through the same ownership rules as long packets, and dispatches OPEN
with the fields and request ID. A provider may alternatively supply a complete
envelope to `open`. Fragmented header storage belongs to the runtime and is
released after dispatch or close; an incomplete opening cannot accept FIN.
Consumers use the same events and packet ownership rules regardless of provider;
a QUIC bearer or a future TCP adapter does not change the consumer interface.
Only the opening request is an envelope; body bytes are opaque.
Consumed counts include header bytes. Rebuild Main alongside providers using
opaque open; older runtimes accept only the complete-envelope form.

DATA borrows bytes until the handler returns the exact consumed count. FIN is a
separate call after all body bytes. FIN closes only the provider-to-consumer
direction: a report/watch consumer may continue writing after it. Full CLOSED
ends both directions. A consumer must never retain a DATA pointer.

`write_pkt` consumes a uniquely owned pool packet only on success. On every
negative return the consumer retains the same packet. One output packet per
stream may be pending. `take` moves that exact slot into the provider's packet
ownership; quota pressure leaves it pending. `written` permits another output
once the provider owns the bytes for reliable delivery. It is not a peer ACK.
Closing or stopping either instance invalidates the handle and releases native
pending packets. Providers receive no SDK callbacks or old module pointers.
