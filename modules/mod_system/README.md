# System diagnostics

Use `dmesh-cli DEVICE system.heap` for memory usage, `system.heap_dump --kind=0`
for regions, `--kind=1` for blocks, and `system.dump_tasks` for tasks and stack
usage. Heap trace kinds are 3 (start), 4 (dump), 5 (stop); kind 6 streams CBOR
block records. See [API.md](API.md).

The installed module auto-starts and exposes its generated native table and
remote methods. No timers, heap allocations or private caches are retained by
the module. Diagnostic operations use the existing runtime mechanisms.
Power information, sleep and PM-lock diagnostics remain in NAN.
