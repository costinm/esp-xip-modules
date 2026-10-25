# Stage2 (second stage bootloader)

Stage2 replaces the ESP-IDF bootloader - its purpose is to allow a smaller image ('loader') with minimal
code to upgrade the 'main' image which is larger. This is better than A/B OTA because loader can use less
flash.

It decides what to run from the RTC handoff byte, set by Main when it wants to be upgraded, as well 
as checking quick resets and watchdog resets. 

It maps Main or loader (tests QIO, falls back to DIO for better speed) and boots it.

```sh
scripts/build.sh esp32c6                   # also writes target/out/esp32c6/stage2-4mb.bin, wl-boot-4mb.bin and partition-table-4mb.bin
XIP_FLASH_MB=16 scripts/build.sh esp32s3   # boards with 16 MB flash: the 16mb table and a 16 MB flash size setting
```

The partition table must match the board's flash: loader and main do not need to know the partition table and
are compiled with the 4M (because compiler requires it even if it is not used).

For S3, `XIP_FLASH_MB=16` also enables the PSRAM that ships with the 16 MB modules (`XIP_PSRAM=0` opts out).

Deploy with `scripts/flash-serial.sh PORT --chip CHIP --table 4mb` (choose the
board capacity). It writes Stage2 at 0x0 (0x1000 on ESP32), the table at 0x8000,
`wl-boot` at 0x10000, C Main at 0xf0000 and modules using the selected table. Parts:

| file | what |
|---|---|
| `CMakeLists.txt`, `bootloader_components/main/` | the bootloader component (`bootloader_start.c`, Kconfig: entry button, reset counts) |
| `sdkconfig.defaults`, `sdkconfig.<chip>.defaults` | per chip; they reserve 0x20 bytes of RTC memory for the handoff |
| `partitions/4mb.csv`, `8mb.csv`, `16mb.csv` | `nvs` 0x9000, `phy_init`, `loader` 0x10000 (0xe0000, holds the loader state journal in its last 8 KB), `main` 0xf0000 (0x190000 in all three), `data` 0x280000 to the end of the flash (modules, 64 KB pages) |

`partitions.csv` at the top of this repository is only the build-time template for loader and main - the real table on a board is the one Stage2 wrote. 

The C-only layout leaves 24, 88 or 216 module pages on 4, 8 or 16 MB boards.
The module slots can also be used for data.
