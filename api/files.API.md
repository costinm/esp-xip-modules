# `files` API (15)

Direct packet filesystem service for modules and runtime clients. The runtime
routes this component to its registered provider; mod_flash owns the storage,
flash operations and collection. No stream, sys module or transport is
needed. An absent provider returns NOT_FOUND. Settings may use this service;
NVS migration is separate from this API.

Send one CBOR request to component 15. Replies return the same component, method
and correlation id. Bare negative ABI integers are errors (NOSPACE=-20,
VERSION=-21). Content is bytes, at most 1024 per call, never silently truncated.
Names are 1–8 opaque bytes. Larger files can be read in ranges, with expect set
to the first reply's version to avoid mixing replacements. Named content is at
most 65504 bytes. The provider serializes writes with stream operations, queues
up to four waiting direct requests, and returns BUSY beyond that. Replies to
flash writes follow durable record/index commit; memory writes publish their replacement
when complete. Queue storage is released on stop.

Metadata operations below share the field tags of the flash stream API.

## 2. `file.stat` — inspect a named record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 opaque bytes, zero padded. |
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |

Response `{1: version, 2: size, 3: kind, 4: cache}`.

## 5. `file.remove` — remove all live versions

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | Opaque name. |
| 5 | `expect` | `u32` | Optional current-version check. |
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |

Older copies are cleared before the newest. Response is an empty map.

## 6. `file.list` — page through names

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `after` | `bytes` | Optional exclusive lower name bound. |
| 2 | `max` | `u32` | Default 16, capped at 32. |
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |

Response `{1: entries, 2: more}`. Each entry is `{1: name, 2: version, 3: size,
4: kind, 5: cache}`. Names are sorted bytewise.

## 7. `file.sync` — wait for committed writes

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |


No required fields; response is empty. Every successful flash write already waits
for its record and index entry to reach flash. This is a barrier on the serialized operation path.

## 8. `fs.info` — allocation summary

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |


No required fields. Flash response `{1: free_pages, 2: dirty_pages, 3: max_file}`. Counts cover
the back region; the current in-memory image allocation is excluded from available pages.

## 12. `log.erase` — acknowledge batches

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 10 | `through_seq` | `u64` | Only whole batches ending at or below this sequence. |

Erase whole eligible telemetry D pages; otherwise clear individual batch live
bits. Never erase a page containing retained named records. Empty response.

## 17. `file.get` — read a committed range

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 opaque bytes. |
| 5 | `expect` | `u32` | Optional expected version. |
| 6 | `offset` | `u32` | Default zero. |
| 7 | `len` | `u32` | Default 1024; at most 1024. |
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |

Reply `{1: version, 2: total_size, 3: kind, 4: cache, 6: offset, 13: data}`.
A range beyond EOF returns empty bytes with offset clamped to EOF.

## 18. `file.put` — replace a small file atomically

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 opaque bytes. |
| 3 | `kind` | `u32` | Default 1; 1–3. |
| 4 | `cache` | `bool` | Default false. |
| 5 | `expect` | `u32` | Optional version check; zero requires absence. |
| 9 | `version` | `u32` | Optional positive version, greater than the current file version; otherwise increment automatically. |
| 13 | `data` | `bytes` | Entire replacement; at most 1024 bytes. |
| 16 | `namespace` | `u8` | Default 0 (flash); 1 RAM, 2 PSRAM, 3 RTC, 4 proc, 5 sys. |

Reply `{1: version, 2: size, 3: kind, 4: cache}` after commit. An unconditional
retry may create a new version. No partial file writes are exposed.

## 19. `log.put` — append a small telemetry batch

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 10 | `first_seq` | `u64` | First producer sequence number. |
| 11 | `count` | `u32` | Number of CBOR items; 1–65535. |
| 13 | `data` | `bytes` | Encoded batch; at most 1024 bytes. |

Reply `{1: newest}` after commit. Same numbering and acknowledgement rules as
flash.log.append; content bytes are not interpreted.

## 20. `log.get` — read whole telemetry batches

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 10 | `since_seq` | `u64` | Last acknowledged sequence; default zero. |
| 13 | `max` | `u32` | Content budget; default/max 1024. |

Reply `{1: newest, 2: oldest, 14: [{1: first_seq, 2: count, 13: data}, ...]}`.
Batches with items after since_seq are returned oldest first, including the
whole overlapping batch. Stop before a batch exceeding either the content
budget or the reply packet capacity; never skip it or truncate its content.
An oversized batch is available through flash.log.read. Empty history reports
zero for oldest/newest. fs.info includes `{4: local_max}`.

## Memory namespaces

Tag 16 is a one-byte namespace ID; omitted/0 preserves existing flash behavior.
IDs 1 RAM and 2 PSRAM allocate contents lazily; PSRAM returns NOMEM when unavailable.
ID 3 RTC uses a packed append log with linear scans, variable-size records and
in-place compaction when necessary. It has no fixed file count or per-file slot.
RTC is best effort: interruption during compaction can lose RTC files. These namespaces accept 1–6 nonzero name bytes,
including four-byte names. Versions increment on writes; an explicit version must
be higher. Streamed writes go directly to the pending allocation (an appended RTC record for RTC), published only
on exact-length FIN. Interrupted writes preserve the prior file. Reads pin their
allocation; replacement/removal while pinned returns BUSY.

ID 4 proc lists read-only `status` (CBOR device snapshot) and `panic` (pending CBOR
panic diagnostic, empty if none). ID 5 sys lists `quar`: read its CBOR list of disabled
module names; write raw module-name bytes to clear one quarantine, without starting
it or deleting the diagnostic. It also lists `panic`: read the retained panic, or
write the exact archived CBOR record to acknowledge and clear only that matching
diagnostic. A different newer panic returns VERSION. An already empty diagnostic
succeeds; quarantine is not changed. Generated file versions/sizes in listings are zero;
stat/read snapshots provide their current sizes. Non-flash files are marked cache
and are excluded from ordinary persistent-file reconciliation. Logs and image
methods remain flash-only. There is no general path parser or arbitrary memory access.

Local callers may obtain `get_component("memory-files")`, a version-1 MemoryFiles
table, to open a read lease returning MemoryFile.data/len without copying. Close
the token before unloading the caller or releasing its context. Do not retain the
address after close, put it on the wire, or persist pointers in RTC. The table also
supports replacement allocations, explicit commit/abort, listing and removal.
RAM/PSRAM are volatile. RTC storage survives software restart/deep sleep, subject
to retention power; it is cleared on power-on or incompatible format. Main reserves
6 KiB for panic/quarantine and RTC files together, in slow RTC on ESP32/S3 and LP
RAM on C6. IDF bootloader/timer/wake reservations remain separate.

RAM/PSRAM blobs are not limited to the flash record's 65504-byte maximum: capacity
is limited by available heap and the u32 length interface. Contents outlive an
invocation, until explicitly removed or reset. The RAM directory currently uses
a linked list; large blob size does not add lookup cost. A hashmap can replace
that directory without moving the separately allocated blobs if entry count grows.
Native open mode 3 pins an existing RAM/PSRAM blob for in-place editing without
copying or allocating a replacement. Release advances its version, including on
abort: in-place edits cannot be rolled back. Remote writes remain replacements.
RTC compaction is blocked while an RTC address is borrowed. There are no RTC RAM
index nodes between calls. The best-effort format may be cleared on upgrade.
