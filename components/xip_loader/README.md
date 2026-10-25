# XIP loader

Maps and starts independently packaged modules. Applications use `xip_runtime`
to provide the module ABI and lifecycle services.

Build modules with `scripts/build.sh <chip>`. `modules/SLOTS` assigns each module
a starting slot and an optional span, for example `mod_nan 7 2`. On ESP32 and
ESP32-S3, each slot provides a 64 KiB link window; larger modules use consecutive
windows. The module uses only its starting slot's state cell.

Flash placement is independent of link slots. An image occupies enough
consecutive 64 KiB flash pages for its actual size. The loader rejects overlapping
active link windows and images that exceed the available mapping or flash space.

Per module name, the highest installed version is selected. Older versions remain
shadowed for recovery and later reuse. Use [mod_flash](../../modules/mod_flash/README.md)
for uploads; a larger image needs enough consecutive free or reusable pages.

See the [current module guide](../../notes/ai/modules/README.md) and
[inspection/recovery commands](../../notes/ai/modules/TROUBLESHOOTING.md).
