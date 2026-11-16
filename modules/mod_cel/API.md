# CEL module API (experimental)

Component 53, version 1, method 1 evaluates a binary uCEL ruleset. 53 is a
provisional module-local assignment: repository search found Berry 50, wasm3 51
and mux tag 52, and no component/tag/wire assignment for 53. It is not a
centrally registered schema allocation and must be reviewed before deployment.

Request CBOR map: field 1 is a byte string containing the uCEL CBOR array of
expressions; optional field 2 is a byte string containing the uCEL CBOR context
map (default empty). Response map: field 1 is an array of scalar results, field
2 is a byte string containing the final encoded context. Invalid requests,
evaluation failures and scheduling failures return ERR_ARG. Binary, context and
response buffers are limited to 16384 bytes; actual transport packet limits can
be smaller. uCEL CBOR contexts represent nonnegative signed integers as unsigned
on decoding. Rules run sequentially with one context and 4096 fuel per run.

Functions: `set(uint_tag, value)` updates the shared context and returns value;
`log(string)` logs and returns null; `now_ms()` returns a uint when the clock
fits in u32, otherwise fails. `after(uint_ms)` returns null and requests a fresh
run of the same binary rules. Delay must be positive; zero is rejected to avoid
unbounded same-time event loops. At most eight after calls per run, subject to
host alarm availability; at most eight alarms may be pending per module instance
across all requests. The final context must fit in 16384 encoded bytes before
alarms are installed. Alarms are installed only after successful evaluation.
Each gets an independent snapshot of the **final** context of that evaluation,
including mutations after the after call. Subsequent requests and sibling alarm
runs do not share changes. Each fresh run has a new fuel budget and may schedule
more runs. Stop cancels pending alarms. Log and other completed side effects are
not rolled back on failure; results of scheduled runs are not returned, and a
failed scheduled evaluation is logged.

Method 2 asynchronously loads binary rules from files component 15, method 17.
Its request map contains field 1, a nonempty filename byte string of at most
8 bytes, and optional field 2, the encoded context byte string. Its eventual
response has the same shape as method 1. One load may be pending per instance;
another returns ERR_BUSY. Loads use 1024-byte version-checked ranges, accept at
most 16384 file bytes, and time out after 30000 ms. File errors are returned to
the caller; malformed range replies return ERR_IO and oversized files ERR_SIZE.
Stop cancels a pending load and its timeout without replying. Actual packet
limits may be smaller than these logical limits.

Expressions cannot make asynchronous or synchronous component requests. Rules
may be compiled externally using uCEL's optional parser; source compilation is
not included in the module. The module has not been tested as a deployed flat
image.
