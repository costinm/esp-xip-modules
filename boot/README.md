# Stage2 (second stage bootloader)

Stage2 replaces the ESP-IDF bootloader. After the first 64 KiB (Stage2, table, `nvs`, `wlstate`, `phy_init`) the flash is ONE partition,
`xip`, to the end of the flash. 

Inside the partition, 64k 'extents' are used for the executable image, modules and data.

* **Extents.** Stage2 finds the runtimes with the extent scan (`components/xip_loader/include/xip_extent.h`, shared with the loader and the host
  tests; `mod_flash` has its own implementation of the same format). It reads 8 bytes at the start of each 64 KiB page of `xip`: `0xFF` empty,
  `0x00` deleted, `M` a module and `D` a data extent (both carry their length at bytes 4-7 and are skipped by it), `0xE9` an ESP image, a runtime
  (its length is the end of its last segment, its version the app description's). The first two runtimes by address are considered. Where
  they sit is the allocator's policy (mod_flash): A at the start of `xip` (0x10000), B in its last 1.5 MiB.
* **Version.** Each runtime image carries a number, the leading digits of the version string of its app description (`scripts/build.sh`
  sets it to seconds since 2025-01-01, or `XIP_RUNTIME_VERSION`). Stage2 boots the higher one; a tie goes to A.
* **Trial.** Two valid images mean the newer one is on trial. If it crashes (five quick resets, or five watchdog/brown-out resets in the
  longer window) Stage2 boots the older one with reason CRASH in RTC. It writes nothing: the older runtime erases the failed one
  (`xip_loader_reap`, called by `xip_loader_init`, programs 0x00 over the first byte of each of its pages; `mod_flash` erases them when it needs the space).
* **Settled.** A runtime that has stayed up (`xip_loader_settle_after`, 60 s in `main/`) may erase the older image. One valid image is a settled runtime and a crash loop has nothing to fall back to: the runtime then starts in **safe mode** (below); esptool is the last resort.
* **Safe mode.** A crash verdict (reason CRASH in RTC) with no failed newer runtime to reap means the running runtime is the one crashing. `xip_loader_init` sets `xip_loader_safe_mode()`; `xip_loader_autostart` then starts only the modules in the config's `safe_modules` (`main/`: `flash`) and `main/` leaves out BLE, the QUIC classifier and the settle timer. Wi-Fi in station mode (saved credentials) stays so a good runtime can be uploaded. Which modules belong in the list is open. The next reset after the quick window boots normally, so a persistent crash cycles: 5 quick resets, one safe boot. Tested on a C6 (five software resets, then `flash` alone).
* **Entry button / RTC handoff `LOADER` / NVS `stg2:boot_target=2`** boot the older image without erasing anything (a manual rollback).
  `WL_HANDOFF_MAIN` and `boot_target=1` boot the newest.
* **Manual flash.** Until the extent header exists (module header format at a fixed place in the runtime and in every module, one scan shared
  by Stage2 and `mod_flash`) everything is written with esptool: the runtime at 0x10000 (A), the modules at the addresses of `modules.txt`
  (from 0x190000). Writing B means moving the modules out of its pages first: not automated yet. See the notes,
  `ai/modules/2026-10-31-flash-pool-and-extents.md`.

It maps the runtime (tests QIO, falls back to DIO for better speed) and boots it. The QIO verdict is the only thing it writes, once, in the two sectors of the `wlstate` partition (0xd000 on every chip).

```sh
scripts/build.sh esp32c6                   # also writes target/out/esp32c6/main.bin, stage2-4mb.bin and partition-table-4mb.bin
XIP_FLASH_MB=16 scripts/build.sh esp32s3   # boards with 16 MB flash: the 16mb table and a 16 MB flash size setting
```

The partition table must match the board's flash. The runtime does not need to know it and is compiled with the 4M template
(`partitions.csv`, because the compiler requires one even if it is not used).

For S3, `XIP_FLASH_MB=16` also enables the PSRAM that ships with the 16 MB modules (`XIP_PSRAM=0` opts out).

Deploy by writing Stage2 at 0x0 (0x1000 on ESP32), the table at 0x8000, `main.bin` at 0x10000 and the modules at the addresses
of `target/out/<chip>/modules.txt` (flash addresses: the pages after the reserve, `modules/CRITICAL` first). Parts:

| file | what |
|---|---|
| `CMakeLists.txt`, `bootloader_components/main/` | the bootloader component (`bootloader_start.c`, Kconfig: entry button, reset counts) |
| `sdkconfig.defaults`, `sdkconfig.<chip>.defaults` | per chip; they reserve 0x20 bytes of RTC memory for the handoff |
| `partitions/4mb.csv`, `8mb.csv`, `16mb.csv` | `nvs` 0x9000 (16 KiB), `wlstate` 0xd000 (8 KiB), `phy_init` 0xf000, `xip` 0x10000 to the end of the flash (0x3f0000, 0x7f0000, 0xff0000 bytes) |

`partitions.csv` at the top of this repository is only the build-time template for the runtime - the real table on a board is the one Stage2 wrote.

Module space is the `xip` pages after slot A, minus B while it holds an image: 37, 101 or 229 pages on 4, 8 or 16 MB boards with B empty
(today's modules need 18). The part of slot A the runtime does not use (a 1.25 MiB runtime leaves 4 pages) is idle until the extent scan can use it.

`wlstate` (0xd000, 8 KiB) is Stage2's own state log: two 4 KiB sectors used as an append-only record log (NOR flash turns bits 1->0 without an
erase, so each record goes into the next erased slot). It holds the QIO verdict (the only record Stage2 writes) and the room for pinned keys
and minimum versions (`WL_REC_*` in `esp_wifi_loader_state.h`). It is separate from `nvs` because Stage2 reads it without the NVS code.
