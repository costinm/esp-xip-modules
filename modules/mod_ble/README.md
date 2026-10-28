# mod_ble

BLE companion policy over native NimBLE mechanisms. See [API.md](API.md).

## Retained state

The 128-byte state block holds the hosted BLE policy table only. The module
keeps no scan rows, SDUs or request buffers between calls. Native NimBLE owns
controller/host tasks, pairing, advertising/scanning state and CoC buffers while
those roles are enabled; these costs belong in the device baseline and are not
zero merely because this adapter is idle. Disable the native role before
measuring its teardown.

Verify the declared state/stack budget with `python3 scripts/check-module-memory.py`
from the repository root. For device heap/packet lifetime measurements, use the
[common measurement procedure](../README.md#measuring-retained-ram).
