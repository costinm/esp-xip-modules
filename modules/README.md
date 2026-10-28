# XIP modules

Every module README describes its retained state: RAM that survives a handler
return, why it is needed, and which operation ends its lifetime. A fixed array
inside a heap-allocated state/context still consumes heap. Moving a buffer
from stack to permanent state does not make its inactive cost zero.

Optional features should retain only a small descriptor when disabled. Payloads,
observation history and scratch space belong to an active operation or to pool
packets with explicit ownership. Successful stream writes transfer ownership;
the stream returns the lease after consumption. A refused write leaves the
lease with the caller. A module must not free a successfully transferred lease.

`retained-memory.json` caps each image's declared state and stack reservations.
`python3 scripts/check-module-memory.py` checks those caps and requires a
retained-state section for every module. It runs in the host generated/ABI test
group. These budgets do not measure managed contexts, native resources, allocator
overhead or shared packet retention. Document and measure those separately.

## Measuring retained RAM

1. Source `./env.sh`, select one board with dmesh-cli and record its running
   Main/module versions. Keep one serial owner and use C Main plus modules.
2. Warm up the transport once, then record `system.heap` (free/internal free
   and largest block), `system.dump_tasks` (stack use) and `console.status`
   (module and shared packet-queue state). Save structured results on the host.
3. Enable allocation tracing with `system.heap_dump --kind=3` when the build
   supports it. Run one feature session, close/cancel it, wait for its deadline
   and transport output to drain, then dump/stop tracing with kinds 4/5. Use
   kind 6 for a bounded heap block snapshot over PPP. A dump is a diagnostic
   result, not a per-module allocation ledger.
4. Repeat at least three times. Compare steady-state free bytes and outstanding
   packet state to the warm baseline, rather than minimum-free (a historical
   low-water mark). Account separately for transport associations and native
   radio/BLE roles that remain enabled. Compare after stop/unload as well.
5. Test a slow/blocked reader, early disconnect, timeout, malformed input and
   stop during the operation. Once the stream/provider drains, no observation
   packet or session allocation should remain. Host fixtures can count exact
   allocation deltas and assert ownership; total device heap alone cannot
   attribute a leak to a particular module.

For NAN, `discovery.nodes` opens a five-second live window and triggers active
discovery. Compare before, during and after that window; opening another stream
while it is active returns BUSY. Empty and closed windows retain no history.
Optional publication bytes must return to zero capacity when cleared.
There is no separate nan.watch observation registration.

Current audited costs that are not near zero: LoRa's inline TX arrays remain
inside its 768-byte state, even while suspended; flash's autostarted listener
and polling alarm remain resident; started QUIC associations can retain reusable
capacity. The individual READMEs identify their owners and teardown boundaries.
Image state/stack caps prevent silent growth, but do not imply those existing
baselines have been eliminated.
