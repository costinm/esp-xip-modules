## Future: flat images mapped like XIP

Today the host-runtime loads modules as shared objects (`.so`). Each instance gets its own file copy so it has its own
statics. That works, but it is not XIP: the dynamic loader relocates the object and keeps a writable data segment, and
separate copies do not share text pages.

Plan: build modules for the host as **flat, position-independent images** and `mmap` them read-only.

- **Image:** the same 64-byte header and `entry_offset` as the device images, linked with a tiny linker script and
  `objcopy -O binary`. No relocations, no writable data, no libc: the rules `mkmod.py` already enforces for the device
  targets. Code uses rip-relative addressing (position-independent, small code model); no function-pointer tables in
  read-only data.
- **Loading:** `mmap(MAP_PRIVATE, PROT_READ|PROT_EXEC)` at any address, after the same header checks as `xip_check.c`,
  then call `entry_offset`. Nothing is relocated or copied, pages are file-backed (the kernel can page them out and in),
  and instances of one module share the same pages.
- **Benefit:** the host exercises the real image format and header validation, which the `.so` path skips.
- **Blocker:** `xip-abi`'s non-device allocator binding (`tp_swap` in `heap.rs`) uses a `static AtomicUsize`, which is
  writable data. It has to move into the host table or a field reachable from the `Host` pointer.
- **Work:** a host target in `tools/mkmod.py` and `mmap` loading in `Loaded`. A host image is x86-64 with 8-byte pointers,
  so it is not a device image.
- **Not tried yet:** none of this has been built or measured; whether it is cheaper to page than the `.so` is open.

Alternative, not planned: run the device RISC-V `.dmod` itself under an emulator and trap the host-table calls. It runs
the exact device bytes but is slower and not demand-paged, and Xtensa images are harder.