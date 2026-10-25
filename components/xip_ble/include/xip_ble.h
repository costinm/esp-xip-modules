/* SPDX-License-Identifier: Apache-2.0 */
/* The NimBLE component of the runtime (`nimble`, include/xip.h `xip_nimble`): a thin subset of the NimBLE host, moved from fw/ble
 * (dmesh_nimble.c). Advertising of the data the caller gives, passive scan with the caller's filters, one LE CoC channel (server or client), the
 * bond store and the wake/sleep modes. All policy (UUIDs, advertisement layout, name, PSM, pool size) is the caller's: `mod_ble` hosts the `ble`
 * component (104) over this one; see crates/xip-abi/src/nimble.rs.
 *
 * Nothing starts by itself: xip_ble_install() only offers the table (get_component("nimble")), the bearer hooks and the power hooks; the
 * NimBLE host is started by the first start/listen/scan/connect call. With CONFIG_XIP_BLE=n every function below is a stub, the table is
 * not offered and no BT code is linked. */
#pragma once

#include <stdbool.h>
#include <stdint.h>
#include "xip.h"

#ifdef __cplusplus
extern "C" {
#endif

/* The component table (what get_component("nimble") returns), NULL when built out or released. */
const xip_nimble *xip_ble_component(void);

/* Offer the component: register "ble" with the runtime's get_component, the power hooks (veto and re-arm around a sleep) and the native link "ble"
 * (the CoC channel; received SDUs are runtime packets, shown to the application's xip_quic_rx_set function; send with xip_link_native_send). Idempotent. */
int xip_ble_install(void);

bool xip_ble_coc_connected(void);
bool xip_ble_coc_tx_pending(void);

/* The application's boot policy flag (from its settings); with CONFIG_XIP_BLE_RELEASE_WHEN_OFF a false value releases the controller
 * memory for this boot (BLE then needs a reboot with the flag on). */
void xip_ble_configure_boot(bool enabled);
bool xip_ble_boot_enabled(void);

#ifdef __cplusplus
}
#endif
