/* SPDX-License-Identifier: Apache-2.0 */
/* Installing signed module packages: the module sink of esp-wifi-loader (esp_wifi_loader_serve) over the slot calls of
 * espmod_loader. esp-wifi-loader verifies everything (pinned key, signature, block table, every block); this knows what
 * it does not: which slot a module name is, the module's own image header, and the minimum version kept in the
 * loader's state log. Call after espmod_loader_init(). An application that installs images itself does not use it. */
#pragma once
#include "esp_err.h"
#include "esp_wifi_loader.h"

#ifdef __cplusplus
extern "C" {
#endif

esp_err_t espmod_install_init(void);
/* The sink itself (what esp_wifi_loader_serve drives), for tests. */
const esp_wifi_loader_module_sink_t *espmod_install_sink(void);

#ifdef __cplusplus
}
#endif
