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
writes follow durable record/index commit. Queue storage is released on stop.

Metadata operations below share the field tags of the flash stream API.

## 2. `file.stat` — inspect a named record

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `name` | `bytes` | 1–8 opaque bytes, zero padded. |

Response `{1: version, 2: size, 3: kind, 4: cache}`.

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

Empty request. Response `{1: free_pages, 2: dirty_pages, 3: max_file}`. Counts cover
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
