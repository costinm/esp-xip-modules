# mod_flash: flash a device over TCP (development)

Component 44, name `flash`. A module that writes Main, a module, the loader, Stage2 or raw flash from an upload on TCP 5227
(manifest stream port 2, `PORT_NET + 2`; the runtime listens from boot, `--autostart`). One connection at a time.
`no_std`, depends on `xip-abi` only; SHA-256 and P-256 come from the runtime `psa` component.

It is a **development tool**. A production device either ships signed modules (this module with the default, strict, authentication) or
does not ship it and lets the loader project reflash Main and modules. There is no hot reload: the module being replaced is stopped, its
page erased and written, and the device restarts for Main/loader/Stage2.

## Package

The format is the loader project's signed package (kept byte compatible, see `docs/development.md`, "Formats"):

```
"WLU1" format flags op ...(16) | public key (65) | signature (64) | header (216) | block table (8 per 4 KiB) | image
```

Target 1 Main, 2 loader, 3 Stage2, 4 module (name in the header), 5 raw flash (name = hex address). A reply is `OK <size>\n` or `ERR <code> <reason>\n`.
Checks in order: signature (see below), chip, rollback version, block table, every block hash, whole-image hash, end of stream.
Both 4 KiB buffers are allocated before the erase; the state journal is written before block zero (the image head), so an interrupted
upload never leaves a bootable half image.

## Authentication

The mode is the request of `start`; it is not stored, so a reboot starts the module strict again (binary `console.stop` followed by `console.start` with name `flash` and request bytes `unsigned`; see `notes/docs/flashing.md`):

| request | meaning |
|---|---|
| empty (autostart, default) | signed by the key pinned in the loader state journal (strict; the key must have been pinned before) |
| `unsigned` | no signature or key check (send zero key and signature); hashes are still checked; no rollback check |
| 130 hex digits | accept packages signed by this public key (uncompressed P-256 point) instead of the pinned key; nothing is pinned; no rollback check |

`tools/flash_upload.py` builds unsigned packages and sends them.

## Where a module goes

The module partition `data` has 64 KB pages; the loader treats a page that starts with the image header as a module. The module named in the
package goes to the page that already holds it, else to the first page that is not a module. It must have the same component number as
the installed one, and never 44 (this module cannot replace itself). Minimum versions are kept in the loader state journal (eight entries:
the page number). Xtensa images are linked for one page: install a new module there with esptool at its `modules.txt` offset.

The running application is never written: when `device_info` says Main (partition `main`) runs this module, an upload of Main, or raw flash that touches that
partition, is refused with `ERR 10`. (Erasing the code that executes hung the board in the first hardware test.) Replace Main with the loader or esptool;
a spare partition would make it possible.

## Build and test

`scripts/build.sh` builds it with the other modules; `cargo test` in this directory (also run by `scripts/test-host.sh rust`).
Compatibility vectors currently live in the local root `tests/data` tree (`loader-esp32c6.pkg`, `module-lora-esp32c6.pkg`, `state.bin`). Those tests are not in the CI-safe `ci-runtime` group until rewritten with crate-owned fixtures/generated inputs. Firmware CI still builds the module.
