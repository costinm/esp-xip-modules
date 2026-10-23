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
/* Equivalent installer for the Rust extension table, before init_simple. */
void xip_wifi_install_simple(xip_host_ext_t *ext);
/* After host/loader init, before autostart: shared saved-network fallback. */
void xip_wifi_boot_network(void);
int xip_wifi_nv(const char *key, size_t key_len, const uint8_t *value, size_t value_len,
                   uint8_t *out, size_t cap);

/* What the ESP-NOW link needs to know of the radio (xip_link_espnow.c): the channel it is on, the local addresses, whether only the soft
 * AP is up. */
bool xip_wifi_current_channel(uint8_t *channel, wifi_second_chan_t *secondary);
bool xip_wifi_interface_mac(wifi_interface_t interface, uint8_t mac[6]);
bool xip_wifi_associated(void);
bool xip_wifi_ap_only_active(void);
/* ESP-NOW as the "espnow" link (DMesh's action-frame bearer): started with the radio when the intent wants it
 * (`now`), stopped before it. */
/* Radio owner registers this callback after start; no registration in the bearer. */
int xip_link_espnow_action_rx(void *context, size_t second, uint8_t *third, uint8_t *fourth);
/* Radio-owner-only lifecycle. Wi-Fi must be started and channel selected.
 * now_only means no AP and no intended STA association: only that mode gets
 * a connectionless PM vote. No native initialized flag or MAC crypto policy.
 * Category 127 is a single SDK callback slot, shared by vendor actions. */
int xip_wifi_now_enable(int (*callback)(void *, size_t, uint8_t *, uint8_t *), bool now_only);
int xip_wifi_now_disable(void);
int xip_link_espnow_start(void);
void xip_link_espnow_stop(void);
/* Optional per-enabled-epoch debug counters (CONFIG_XIP_ESPNOW_DIAGNOSTICS; zeros when off), as the `now` record of the wifi status
 * (api/host.API.md): where a received frame died, without a host attached. */
typedef struct {
    uint32_t dispatcher;           /* the private hook called, past the entry gates (either call form) */
    uint32_t tx_response_hook;     /* the action request's in-band reply hook was called */
    uint32_t actions;              /* delivered to the link */
    uint32_t self_echoes;          /* dropped: source is one of our own addresses */
    uint32_t duplicates;           /* legacy field: deduplication belongs to consumers, zero */
    uint32_t invalid;              /* dropped: a malformed native call */
    uint32_t parse;                /* dropped: the body does not parse */
    uint32_t busy;                 /* legacy field: parser is now reentrant, zero */
    uint32_t drops;                /* every drop above */
    uint32_t tx;                   /* accepted by the driver */
    uint32_t tx_failures;          /* refused by the driver */
    int32_t tx_last_error;         /* the last driver error (ESP_ERR_*) */
    uint32_t hook_calls;           /* private-hook invocations during this enabled epoch */
    uint32_t disabled;             /* legacy field: disabled epochs allocate no counters */
} xip_espnow_stats_t;
void xip_link_espnow_stats(xip_espnow_stats_t *out);
/* Promiscuous receive (the wifi component's `monitor`): the frames go to the "promisc" link. Stopped with the radio. */
/* Shared generic owner entry: validates the ABI and requires a running radio. */
int xip_wifi_owner_monitor(const xip_wifi_monitor *monitor);
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
