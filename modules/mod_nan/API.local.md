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
