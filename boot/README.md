# Stage2 (second stage bootloader)

Stage2 replaces the ESP-IDF bootloader. It reads the partition table, decides what to run (Main at `main`, or the loader at `loader`) from
the RTC handoff byte, the entry button, quick resets and watchdog resets, and maps Main (tests QIO, falls back to DIO).
The same code and partition tables are used by the old loader project (a frozen copy, which owns the loader image itself).

**Rule: a change to `boot/` or to the partition tables is made in both repositories in the same change** (this copy and the loader
project's `boot/`; the two directories must stay identical).

Build (part of `scripts/build.sh`; an ESP-IDF shell, `. ./env.sh`):

```sh
scripts/build.sh esp32c6        # also writes target/out/esp32c6/stage2-4mb.bin and partition-table-4mb.bin
```

Flash a blank board with esptool: Stage2 at 0x0 (0x1000 on the ESP32), the partition table at 0x8000, the loader at 0x10000 (loader
project), `main.bin` at 0xf0000. Parts:

| file | what |
|---|---|
| `CMakeLists.txt`, `bootloader_components/main/` | the bootloader component (`bootloader_start.c`, Kconfig: entry button, reset counts) |
| `sdkconfig.defaults`, `sdkconfig.<chip>.defaults` | per chip; they reserve 0x20 bytes of RTC memory for the handoff |
| `partitions/4mb.csv`, `8mb.csv` | the lab layout: `nvs` 0x9000, `phy_init`, `loader` 0x10000 (0xe0000, holds the loader state journal in its last 8 KB), `main` 0xf0000 (0x290000), `data` 0x380000 (modules, 64 KB pages) |

`partitions.csv` at the top of this repository is only the build-time template of Main (without the `data` entry; `main` matches `4mb.csv`); the real table on a
board is the one Stage2 wrote. `tests/data/partition-4mb.bin` is `4mb.csv` compiled with `gen_esp32part.py`; mod_flash's tests read it.

Differences from the loader project's `boot/`: none in the sources; `state_core.c`, `esp_wifi_loader_state.h` and `esp_wifi_loader_rtc.h` (the state journal and the RTC handoff,
both frozen formats) are copied next to `bootloader_start.c` instead of being taken from the loader's `components/`, so this directory builds alone.
