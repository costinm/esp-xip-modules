/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "esp_wifi.h"
#include "xip_runtime.h"

/* NVS namespace "wl": rx_static, rx_dynamic, tx_dynamic, rx_ba. NVS must
 * already be initialized. Both the C and Rust hosts use this policy. */
void xip_wifi_apply_init_settings(wifi_init_config_t *config);
wifi_init_config_t xip_wifi_init_config(bool ampdu_enabled);

/* Optional, simple STA implementation of the module Wi-Fi ABI. The application
 * opts in by installing these hooks; it owns the radio exclusively while used. */
void xip_wifi_install_hooks(xip_host_hooks_t *hooks);
int xip_wifi_nv(const char *key, size_t key_len, const uint8_t *value, size_t value_len,
                   uint8_t *out, size_t cap);

/* What the ESP-NOW link needs to know of the radio (xip_link_espnow.c): the channel it is on, the local addresses, whether only the soft
 * AP is up. */
bool xip_wifi_current_channel(uint8_t *channel, wifi_second_chan_t *secondary);
bool xip_wifi_interface_mac(wifi_interface_t interface, uint8_t mac[6]);
bool xip_wifi_associated(void);
bool xip_wifi_ap_only_active(void);
/* ESP-NOW as the "espnow" link (DMesh's action-frame bearer): started with the radio, stopped before it. */
int xip_link_espnow_start(void);
void xip_link_espnow_stop(void);
/* Promiscuous receive (the wifi component's `monitor`): the frames go to the "promisc" link. Stopped with the radio. */
int xip_link_promisc_set(const xip_wifi_monitor *monitor);
void xip_link_promisc_stop(void);
/* Pin the unassociated radio to a channel (HT20). */
bool xip_wifi_set_channel(uint8_t channel);
/* The early receive filter (xip_rx_filter.c, IRAM): the radio owner sets our MACs and the BSSID. */
void xip_rx_filter_local(const uint8_t *sta, const uint8_t *ap);
void xip_rx_filter_bssid(const uint8_t *bssid);
bool xip_rx_accept(const uint8_t *frame, size_t len);
unsigned xip_rx_filter_dropped(void);
/* Promiscuous counters, for the power component's info. */
unsigned xip_link_promisc_frames(void);
unsigned xip_link_promisc_on(void);
