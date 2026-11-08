# Flash format for boot loader

The stage2 loader is designed around 'A/B safe update' - writing a new image, testing it
and falling back to the previous stable image if it fails. At the end of the upgrade 
the unused image can be deleted and the space used for other purposes.

Given the common 4M partition size and at least 800k for a minimal OS with WiFi, this won't
work well with the fixed partitions used by ESP-IDF, so instead we use a micro-filesystem 
format:
    
- Flash is defined in 64k 'extents' (this is what the ESP32 MMU can map).
- First extent is reserved for the required boot loader, partition table, calibration, OS settings (NVS)
- Rest of the flash may contain 1 or 2 images, each aligned at 64k and contiguous.
- It may contain extents with first byte M and length at offset 4..7 - like images, may span multiple 64k extents. The length needs to be rounded up to find the number of extents. 
- It may have first byte of (D,X,0x00,0xFF) - for single-extent data, index, deleted and erased blocks used by the filesystem. The format is defined by the filesystem - bootloader needs to
now how to skip the blocks.

The loader knows how to parse the ESP image format to determine the length of the file.

The semantics of the other blocks are not relevant for the stage2 bootloader.

## The byte after the first image

If the partition starts with 0xE9, the bootloader parses the image length as usual and reads the first byte after the image
(the length rounded up to 4). If it is in 0xF0..0xFE, the bootloader stops scanning: the rest of the partition is not parsed, and
the first image is run. 0xFF (blank flash) and any other value (a record, a `D` header, 0x00, 0x7C, ...) and scanning goes on as
described above. A runtime that is not at the start of the partition is not checked.

The bootloader attaches no other meaning to the byte; the runtime and the modules use it:

* `0xFE` - fresh boot: the rest of the partition may hold random data and is not formatted.
* `0xFC` - recovery boot: the rest of the partition holds extents, but the space up to 1.5 MiB (`RESERVE`) may hold fragments of
  a previous image and may need to be erased.
* `0xF8` - done: the formatting or recovery was done, and the partition is just this single image (the bootloader still stops).

The bootloader does not depend on the runtime or on `mod_flash`: any valid ESP program can be flashed and used, and it only has to
know when to stop. `mod_flash` formats or cleans the rest as needed and then clears the marker to `0xF8` (programming only clears
bits). When it installs a second image it clears the top bit (`0xF8` to `0x78`), so the bootloader scans the whole partition from
then on.

## Using esptool

The flasher writes the image followed by the marker byte (padded to 4 with 0xFF), because a plain 0xFF after the image means
"keep scanning". For an initial flash erase the partition and add `0xFE`. For recovery erase only the blocks the image needs and
add `0xFC`; the bootloader runs the first image without looking at the rest of the flash, and `mod_flash` then cleans the space
up to 1.5 MiB (see flash-format.md, Placement).
