# Native NAN policy calls

The remote discovery/watch API retains its request context for deferred replies.
These local calls share the same policy implementation and are serialized by the
runtime hosted-component thunks. No borrowed pointer survives the call.

## `usb` — USB attachment

**C function:** `(connected: u32)`

The runtime worker calls this after USB SOF connection state changes, to keep active roles while a host is attached by default, or follow the explicit USB detach policy.

## `wake` — sleep completion or UART activity

**C function:** `(cause: i32)`

The runtime worker calls this with the native wake cause so NAN restores receive capture and active transport policy after local serial or decoded directed-packet activity.

## `sleepy` — explicit sleep policy

**C function:** `(enabled: u32, cadence: u32, trace: u32, immediate: u32)`

Local policy callers and the remote sleepy handler use this to select paired discovery windows; USB attachment vetoes enabling.

## `set_announce` — replace opaque publication

**C function:** `(record: *const u8, len: usize)`

A publication supplier calls this to replace the cache without restarting synchronization or waiting for a message round trip.

## `transport_set` — coordinated transport selection

**C function:** `(request: *const NanTransport)`

Local control callers use this to apply validated independent Wi-Fi, UART, BLE and LoRa/FSK settings; direct transport.set requests use the same NAN policy.

## Module datagrams

The module-owned Rust contract is exported by `src/api.rs`; consumers can depend on
`esp-mod-nan` with `default-features = false, features = ["api-only"]`, or
include that source directly. Requests use component 46 via existing asynchronous
module messages. Remote envelopes and link callers are rejected. One consumer may
subscribe at a time; no xip-abi extension is required.

- Method 18 (`SUBSCRIBE`): `{1: enabled: bool, 2: callback_method: int}`.
  A negative callback method is required on enable: native module messages with
  a negative method cannot auto-activate a stopped consumer. This avoids a late
  observation restarting a module while its asynchronous unsubscribe is pending. The sender becomes the consumer. Another owner
  gets BUSY. Disable frees subscription, pending schedules and prepared leases.
- Method 17 (`SOLICIT`): `{1: payload: bytes}` for broadcast or
  `{1: payload, 2: peer_mac: bytes, 3: instance: uint}` for unicast.
  Payload is opaque, nonempty and at most 231 bytes. Only the subscriber may submit.
  Optional tag 4 (`delay_ms`, 0..64) delays the first submission for flood jitter.
  Four schedules maximum; identical pending submissions reuse the schedule.
  Empty result acknowledges admission, not delivery.
- Callback (`Received`): `{1: peer_mac, 2: instance, 3: now: bool, 4: channel,
  5: signed_rssi, 6: received_us, 7: payload}`. No history or callback retry is retained.
  NAN does not parse the consumer's records or extend awake time for them.

A schedule submits immediately over NOW, once in the current/next NAN DW, and over
both NAN and NAN-over-NOW in DW0 and DW8. Unicast uses Follow-up; broadcast uses
Subscribe Service Info. Scheduling shares the TSF engine, including for local
sleepy cadence 16. Pending schedules may sleep between windows; prepared packet
leases retain their existing BUSY ownership rules. A schedule expires after
8.912896 seconds. STA never leaves its AP channel: unavailable NAN delivery may
expire a schedule after NOW submission. `nan.datagrams` exposes aggregate progress;
completed means all copies were queued, without proving driver or receiver success.
