# Boot: Stage2 and partition tables

Stage2 is the second-stage bootloader that replaces the ESP-IDF bootloader. It
finds the runtime image in the `xip` partition, checks the image version, and
boots the newer valid runtime. The sources here are the Stage2 component
(`bootloader_components/`, `CMakeLists.txt`, `sdkconfig*.defaults`) and the
partition tables (`partitions/4mb.csv`, `8mb.csv`, `16mb.csv`), one per flash size.

Build it with the runtime:

```sh
. ./env.sh
scripts/build.sh esp32c6                 # Stage2, 4 MB partition table and runtime
XIP_FLASH_MB=16 scripts/build.sh esp32s3 # 16 MB boards: 16mb table and flash size
```

The partition table must match the board's flash size. The entry button boots
the older runtime, and five quick resets without an older runtime to fall back
to start safe mode, where only mod_flash receives its onBoot event so the device
can be recovered.

For the flash layout, the module store and the recovery placement, see
[flash format](../notes/docs/flash-format.md). Installing and repairing a board is
in [flash commands](../notes/ai/modules/flash-commands.md).
