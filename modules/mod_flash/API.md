# `flash` API (44)

Stream filesystem and image receiver. Methods 2–15 are the normal API.
The default TCP 5227 listener accepts the same opening CBOR envelope and raw
body as a QUIC stream. On TCP, the declared `size` delimits a write body;
methods without a body finish at the envelope. Read the response until EOF. Each TCP connection carries one operation. The service autostarts,
requests its saved station network and announces on UDP `ff02::5227:5227`.
Recovery discovery and Wi-Fi control also work over PPP port 250 without a
sys module. Main + mod_flash is the complete recovery composition.
The optional `legacy-upload` build feature enables method 1 and TCP 5227
compatibility. In that build, TCP 5227 keeps its existing WLU1 byte
protocol. A QUIC stream starts with the request below, followed by exactly the
same WLU1 module package and FIN. The upload response uses the existing `OK ...` /
`ERR ...` byte protocol. Authentication, hashes, target checks and version
policy are shared across both transports. QUIC currently accepts module updates
only; firmware/recovery updates and replacement of the active stream provider
are rejected.

## 1. `upload` — receive an upload package on a stream

No request fields. The body is a WLU1 package, including its signed header,
block hashes and image. This method is stream-only; it is not a datagram method.

## Stream filesystem methods

Methods 2–15 and 23 are stream-only over TCP or the runtime stream provider. Their opening envelope contains the request fields;
content follows as raw bytes. No WLU1 wrapper, signature, upload manifest or block
hash table is required. Authentication and authorization belong to the caller's
framework. Datagram calls are refused.

A successful response starts with one CBOR map. Reads then send raw bytes, with
lengths given by the map. Errors are `{7: negative_error}`. Writes answer after
flash completion; losing an answer does not undo the commit. One operation is
active at a time. Stream backpressure retains content at the provider: the module
holds at most its current flash packet and three alignment bytes.

## 2. `file.stat` — inspect a named record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 opaque bytes, zero padded. |

Response `{1: version, 2: size, 3: kind, 4: cache}`.

## 3. `file.read` — stream a committed file

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | Opaque name. |
| 5 | `expect` | `u32` | Optional current-version check. |
| 6 | `offset` | `u32` | Default zero. |
| 7 | `len` | `u32` | Default remaining file size. |

Response `{1: version, 2: total_size, 3: kind, 4: cache, 5: body_size,
6: offset}`, followed by exactly body_size raw bytes and FIN. This operation
holds the single operation slot until its stream closes.

## 4. `file.write` — stream a replacement

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | Opaque name. |
| 2 | `size` | `u32` | Exact content length, at most 65504. |
| 3 | `kind` | `u32` | 1 data, 2 program, 3 key-value; default 1. |
| 4 | `cache` | `bool` | Default false; allows dropping at collection. |
| 5 | `expect` | `u32` | Optional version check; zero requires absence. |
| 9 | `version` | `u32` | Optional positive file version/epoch, greater than the current version. |

Exactly size bytes and FIN follow. Without version, the module increments the
current file version (or starts at 1). A supplied version must be greater than
the current version; zero, equal and older versions are refused before allocation. Response
is the same metadata as stat. An unconditional retry may create a new version.
Index publication precedes record commit; superseded copies are cleared oldest
first after the replacement commits. No whole-file staging allocation is made.

## 5. `file.remove` — remove all live versions

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | Opaque name. |
| 5 | `expect` | `u32` | Optional current-version check. |

Older copies are cleared before the newest. Response is an empty map.

## 6. `file.list` — page through names

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `after` | `bytes` | Optional exclusive lower name bound. |
| 2 | `max` | `u32` | Default 16, capped at 32. |

Response `{1: entries, 2: more}`. Each entry is `{1: name, 2: version, 3: size,
4: kind, 5: cache}`. Names are sorted bytewise.

## 7. `file.sync` — wait for committed writes

Empty request and response. Every successful write already waits for its record
and index entry to reach flash. This is a barrier on the serialized operation path.

## 8. `fs.info` — allocation summary

Empty request. Response `{1: free_pages, 2: dirty_pages, 3: max_file,
5: largest_run}`. largest_run is contiguous blank bytes, measured without collection. Counts cover
the back region; the current in-memory image allocation is excluded from available pages.

## 9. `fs.collect` — make a contiguous run or collect all data

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `bytes` | `u32` | Required contiguous bytes in the back region; rounded up to 64 KiB pages. Default zero. |
| 3 | `full` | `bool` | Default false. Visit all original data extents and pack retained files; mutually exclusive with nonzero bytes. |
| 4 | `erase` | `bool` | Default true. False retires source extents without physically erasing them. |

An empty request retains the one-victim operation. With bytes, stop once a
sufficient contiguous run exists; return NOSPACE if installed images or lack of
copy workspace prevent it. Full collection visits each original extent at most
once, without revisiting its compacted output. Retain current non-cache named
files with their versions; cache and telemetry are disposable. Runtime/module
bodies are never moved or retired by this call. Image retirement is explicit.

With erase=false the target run includes dirty, retired pages: it needs erasure
before programming. With erase=true it must be fully blank. A later erasing
collection also sweeps those retired pages; startup may reclaim them too.
Completion follows the final committed relocation and requested erase.

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `free_pages` | `u32` | Immediately usable blank pages. |
| 2 | `dirty_pages` | `u32` | Unallocated pages that need erasure. |
| 3 | `largest_run` | `u32` | Contiguous bytes available with the requested erase policy. |
| 4 | `processed_pages` | `u32` | Source data/orphan pages processed. |
| 5 | `erased_pages` | `u32` | Physically erased pages, including superseded index pages. |

## 10. `log.append` — stream a CBOR batch

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `size` | `u32` | Exact content bytes, at most 65506. |
| 10 | `first_seq` | `u64` | First producer sequence. |
| 11 | `count` | `u32` | Number of items, 1–65535. |

The body is count encoded CBOR items. Flash treats those bytes as opaque; the
producer owns their encoding and sequence allocation. Response `{1: newest}`.
Telemetry D pages do not receive named files.

## 11. `log.read` — stream retained batches

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 10 | `since_seq` | `u64` | Default zero; batches ending after this sequence. |

Each batch is `{1: first_seq, 2: count, 3: body_size}` followed by exactly body_size
raw bytes. A final empty map and FIN terminate the stream. The first batch may
include earlier items. No reader inventory is retained after the stream closes.

## 12. `log.erase` — acknowledge batches

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 10 | `through_seq` | `u64` | Only whole batches ending at or below this sequence. |

Erase whole eligible telemetry D pages; otherwise clear individual batch live
bits. Never erase a page containing retained named records. Empty response.

## 13. `module.write` — stream a module image

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | Module name, matching its embedded manifest identity. |
| 2 | `size` | `u32` | Exact image bytes. |
| 8 | `operation_id` | `u64` | Stable retry identity. |
| 9 | `version` | `u32` | Matching embedded version, greater than zero. |

Raw module image bytes and FIN follow. Claim the M header pending and clear its
pending bit after validation. No operation state is written to flash.
Then request a loader rescan. The module's own embedded header/manifest remain
part of the image format; there is no separate upload manifest.

## 14. `runtime.write` — stream a runtime image

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 2 | `size` | `u32` | Exact ESP image bytes. |
| 8 | `operation_id` | `u64` | Stable retry identity. |
| 9 | `version` | `u32` | Matching application-description version. |
| 12 | `front` | `bool` | Default false. Front installation drops disposable reserve contents. |

Raw ESP image bytes and FIN follow. Verify its segment walk, native checksum and
appended SHA-256 when present. No additional transport hashes are used. At most
two runtimes may be installed. Never erase the executing runtime. A front image
must leave room for its four-byte boot marker. The stream body excludes that
marker; mod_flash writes F8 for a sole front image, or 78 when another runtime
exists. Installing a back runtime clears the front marker's stop bit after
validation.

Module/runtime responses use `{1: version, 2: size, 6: offset}`; offset is relative
to xip and identifies the allocation for later image.remove. Reusing an operation id with matching
parameters reuses the same allocation and restarts from zero while that request
is remembered in RAM. Conflicting parameters for a remembered id are refused.
A different image request replaces the remembered request. After reset, startup
reclaims incomplete images; a retry may choose different blocks. An already
installed image with matching target, name, size and version is returned without
rewriting its body, regardless of operation id. Installation
success does not assert post-reboot application health; the server owns that check.

## 15. `image.remove` — retire an inactive image

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 6 | `offset` | `u32` | Image start relative to xip, 64 KiB aligned. |

Refuse executing runtimes and running modules. Relocate retained named tail
records before retiring the image; cache and telemetry may be dropped. Empty
response. The server controls retirement after checking the replacement's health.

Filesystem-specific errors: -20 NOSPACE, -21 VERSION. Other errors use existing
ABI codes. These are component protocol errors, not additions to the host ABI.

## Direct calls

The runtime files service (component 15) is defined in `api/files.API.md` and
implemented by this module. Methods 17–20 are also accepted on component 44 for
file.get, file.put, log.put and log.get; direct metadata methods 2,5,6,7,8,12
share the existing field tags. Direct content is limited to 1024 bytes, carried
in field 13, and never uses streams. Four waiting direct calls can queue behind
the active operation. fs.info reports local_max as tag 4 for direct callers.


## Recovery Wi-Fi control

These two control methods use raw CBOR envelopes over PPP port 250, including
on Main with its normal request classifier disabled. They
are not file/image streams. UDP multicast remains discovery only; it cannot
set credentials. A PPP request is `{1: 44, 2: method, 3: id, 5: fields}`;
its response is `{1: 44, 2: method, 3: id, 6: result}` or uses `7: error`.

## 21. `wifi.join` — join with temporary credentials

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ssid` | `string` | Network name, 1..32 bytes, no NUL. |
| 2 | `psk` | `string` | Passphrase, 8..63 bytes; empty or absent for open Wi-Fi. |

Credentials are held only in RAM. Neither NVS nor the filesystem is written;
saved boot configuration applies again after reset. The temporary network is
used for reconnect attempts during this boot. Other active radio roles remain
in the intended state.

The response is delayed until the requested station network has an IPv4 address,
up to 60 seconds, without blocking runtime event processing. One join can wait
at a time. A retransmitted PPP request with the same id continues waiting without
extending its deadline; another join gets `ERR_BUSY`. Invalid credentials return
`ERR_ARG`, expiry `ERR_TIMEOUT`. A lost response can be recovered with wifi.info.
A timeout does not revert the temporary intent. Stopping mod_flash cancels the
waiting reply. No PSK is included in responses or logs.

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `ready` | `bool` | Station is associated and has an IPv4 address. |
| 2 | `ssid` | `string` | Currently joined network, empty when unavailable. |
| 3 | `ip` | `bytes` | Four station IPv4 octets, all zero when unavailable. |
| 4 | `port` | `u16` | TCP image/file service port, 5227. |
| 5 | `station` | `u8` | 0 off, 1 joining, 2 associated. |
| 6 | `mac` | `bytes` | Station MAC, six bytes. |
| 7 | `channel` | `u8` | Current Wi-Fi channel. |

## 22. `wifi.info` — inspect the current recovery connection

No request fields. Returns the same fields as wifi.join immediately, including
while a join is pending; does not modify radio state or expose credentials.

### Recovery discovery record

`discover` on UDP/PPP 250 receives a CBOR map. Periodic UDP announcements use
the same record; the recovery announcement is unsigned and identifies a flash
service, not a cryptographic device identity.

| Tag | Value |
|---:|---|
| 0 | Component 44 (record discriminator). |
| 1 | Format version 1. |
| 2 | Runtime image version, u32. |
| 3 | Station MAC identity, six bytes; AP MAC only if station MAC is unavailable. |
| 4 | TCP port, 5227. |
| 5 | Chip: 1 ESP32, 2 ESP32-S3, 3 ESP32-C6. |
| 6 | Station connected, boolean. |
| 7 | Uptime in seconds, u64. |

The encoder borrows one pool packet. UDP borrows its bytes synchronously and
returns the packet afterward; PPP transfers the packet to send, releasing a retained lease on link BUSY. Pool exhaustion
or encoding failure skips the announcement. No text formatter or separate
allocation is used. dmesh-cli renders the record on the host and also accepts
legacy text announcements while older devices are migrated.

### Structured operational diagnostics

Component status `(44,101)` contains CBOR events. Field 0 selects the event;
rendering belongs on the host. No formatted firmware text is emitted for these
mod_flash events.

| Event | Fields |
|---:|---|
| 2 | 1: signed TCP error code. |
| 3 | Panic record as documented by the host API, pending durable persistence. |
| 5 | 1: true when the retained panic was durably logged and acknowledged. |
| 10–15 | 1: uptime milliseconds; saved Wi-Fi applied (10), no saved station (11), temporary join (12), rescue station (13), rescue AP (14), peer closed (15). |
| 16 | 1: upload phase, 2: waiting boolean, 3: bytes fed, 4: base offset, 5: current offset, 6: operation id. |

Status contains the latest event, not a durable event history. Panic persistence
uses the separate telemetry log API.

## 23. `image.list` — inspect installed images

Read-only stream request. One image per response keeps metadata within a packet.

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 6 | `offset` | `u32` | Aligned xip-relative cursor, initially zero. |

### Response

Response `{1: images, 2: next_offset, 3: public_key, 4: xip_base, 5: xip_size,
6: macs}`. macs lists known distinct STA/AP MAC byte strings, or an empty array
when Wi-Fi state is unavailable. Reading it does not enable the radio.
Images is an array of zero or one entry: `{1: kind, 2: name, 3: version,
4: size_bytes, 5: allocated_bytes, 6: offset, 7: running}`. Kind is `runtime`
or `module`; runtime name is empty. Size excludes tail records and runtime boot
markers; allocation is the image's rounded 64 KiB span. Offset is relative to
xip. Running means the currently executing runtime or a running module, not
merely the selected/highest-version image. Shadows are included.

Continue with next_offset until zero (an empty images array). public_key is the
runtime's uncompressed SEC1 public key, or null if identity is unavailable.
This response is an observation over the selected transport, not a signed
identity proof. Reading inventory never erases, formats, collects or rescans.
