# mod_flash

Install signed firmware/module packages over TCP port 5227 during development.
The module accepts one upload at a time and is included in the normal module
build. The default mode requires a trusted signing key provisioned on the device.

From the repository root:

```sh
. ./env.sh
scripts/build.sh esp32c6
python3 tools/flash_upload.py --help
```

Choose authentication when starting `flash` through `console.start`:

| Start request | Mode |
|---|---|
| Empty | Use NV `flash.auth`; unset defaults to `strict`. |
| `strict` | Require the provisioned signing key for this activation. |
| `unsigned` | Allow unsigned development uploads; image hashes are still checked. |
| Public key as 130 hex digits | Require that key for this activation. |

Set NV `flash.auth` to `unsigned` for development uploads across reboots, or
`strict` to require signatures. An empty NV value removes the override and
restores `strict`. The setting takes effect when `flash` next starts; stop/start
it to apply immediately. Explicit start requests override NV for that activation.
See the flashing guide below for the binary console commands and signing tools.

A module upload requires a version higher than the installed one (`ERR 9
rollback` otherwise). It uses consecutive 64 KiB pages according to the image's
actual size, choosing free space first, then wholly reusable shadowed images.
Active images and every page of a running image are protected. The target module
is stopped before commit; a rescan selects the new version and retains the old
one as shadowed.

The current upload journal covers the first eight module-partition pages. If no
suitable consecutive extent fits there, the upload is refused. A larger link-slot
reservation does not reserve physical space for future upgrades.

Uploads cannot replace the executing application or this running flash module.
Use the loader or [serial flasher](../../scripts/flash-serial.sh) where needed;
Main, loader and Stage2 updates require a restart.

See [the flashing guide](../../notes/docs/flashing.md) for package preparation
and upload steps, and [module troubleshooting](../../notes/ai/modules/TROUBLESHOOTING.md)
for placement, inspection and recovery.
