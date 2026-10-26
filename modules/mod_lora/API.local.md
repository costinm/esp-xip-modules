# Native LoRa policy calls

The runtime serializes these calls with radio IRQs, alarms and remote requests.
The LoRa module owns its driver resources; NAN selects when they are active.

## `configure` — apply a radio role

**C function:** `(record: *const u8, len: usize)`

NAN calls this to replace or restore its LoRa/FSK receive role without racing a queued module stop. The borrowed start-format request is validated before stopping existing work; only rx, fsk without payload, and idle are accepted. Success admits asynchronous board initialization, not reception.

## `suspend` — release radio resources

**C function:** `()`

NAN calls this before coordinated sleep or explicit radio off to synchronously cancel radio work and release SPI, IRQ and power. The mapped module remains callable until explicitly stopped.
