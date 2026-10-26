# `stream` API (14)

Local transport-neutral stream delivery for modules. The fixed-layout table is
`Stream` in `crates/xip-abi/src/stream.rs`, obtained with `get_component("stream")`.
It has no remote CBOR methods.

Consumers may bind a component/method with `listen`; their primary component
also accepts streams when the manifest declares a stream port. Providers open
one complete CBOR request envelope. OPEN carries its fields and request ID;
DATA borrows bytes until the handler returns the exact consumed count. FIN is a
separate call after all body bytes. A consumer must never retain a DATA pointer.

`write_pkt` consumes a uniquely owned pool packet only on success. On every
negative return the consumer retains the same packet. One output packet per
stream may be pending. `take` moves that exact slot into the provider's packet
ownership; quota pressure leaves it pending. `written` permits another output
once the provider owns the bytes for reliable delivery. It is not a peer ACK.
Closing or stopping either instance invalidates the handle and releases native
pending packets. Providers receive no SDK callbacks or old module pointers.
