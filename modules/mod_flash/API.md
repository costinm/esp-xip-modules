# `flash` API (44)

Module and firmware package receiver. TCP 5227 keeps its existing WLU1 byte
protocol. A QUIC stream starts with the request below, followed by exactly the
same WLU1 module package and FIN. The upload response uses the existing `OK ...` /
`ERR ...` byte protocol. Authentication, hashes, target checks and version
policy are shared across both transports. QUIC currently accepts module updates
only; firmware/recovery updates and replacement of the active stream provider
are rejected.

## 1. `upload` — receive an upload package on a stream

No request fields. The body is a WLU1 package, including its signed header,
block hashes and image. This method is stream-only; it is not a datagram method.
