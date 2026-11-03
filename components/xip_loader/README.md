# XIP loader

Maps and starts independently packaged modules. Applications use `xip_runtime`
to provide the module ABI and lifecycle services.

Build modules with `scripts/build.sh <chip>`. `modules/SLOTS` assigns each module
a starting slot and an optional span, for example `mod_nan 7 2`. On ESP32 and
ESP32-S3, each slot provides a 64 KiB link window; larger modules use consecutive
windows. The module uses only its starting slot's state cell.

The scan (`xip_extent.h`, shared with Stage2) walks the 64 KiB pages of the `xip` partition: an extent is a runtime (`0xE9`, an ESP image), a module (`M`) or a data extent (`D`); the
first 8 bytes of a module or data extent carry the length; the pages between extents are free. `xip_loader_extents()` returns this map (mod_flash
allocates from it). Runtimes are placed by policy at the start of `xip` (A) and in its last 1.5 MiB (B, `XIP_RUNTIME_RESERVE`). `xip_loader_settle_after` erases the older runtime once the running one has stayed up; `xip_loader_reap` (at init) deletes a newer one that Stage2 rolled back from. A runtime has no deleted state: it is formatted by erasing its first sector, and `mod_flash` sweeps the rest of its pages (its garbage collector runs when the free pages run low or an install needs them). A crash loop with no older runtime puts the loader in safe mode: `xip_loader_safe_mode()`, and `xip_loader_autostart` starts only `safe_modules` of the config. Offsets in the module list are flash addresses. See [internals.md](internals.md).

Flash placement is independent of link slots. An image occupies enough
consecutive 64 KiB flash pages for its actual size. The loader rejects overlapping
active link windows and images that exceed the available mapping or flash space.

Per module name, the highest installed version is selected. Older versions remain
shadowed for recovery and later reuse. Use [mod_flash](../../modules/mod_flash/README.md)
for uploads; a larger image needs enough consecutive free or reusable pages.
