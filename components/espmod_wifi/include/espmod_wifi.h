/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "esp_wifi.h"
#include "espmod_host.h"

/* NVS namespace "wl": rx_static, rx_dynamic, tx_dynamic, rx_ba. NVS must
 * already be initialized. Both the C and Rust hosts use this policy. */
void espmod_wifi_apply_init_settings(wifi_init_config_t *config);
wifi_init_config_t espmod_wifi_init_config(bool ampdu_enabled);

/* Optional, simple STA implementation of the module Wi-Fi ABI. The application
 * opts in by installing these hooks; it owns the radio exclusively while used. */
void espmod_wifi_install_hooks(espmod_host_hooks_t *hooks);
int espmod_wifi_nv(const char *key, size_t key_len, const uint8_t *value, size_t value_len,
                   uint8_t *out, size_t cap);
