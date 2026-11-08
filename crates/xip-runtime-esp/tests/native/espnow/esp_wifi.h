#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "esp_err.h"
typedef enum { WIFI_IF_STA, WIFI_IF_AP } wifi_interface_t;
typedef enum { WIFI_SECOND_CHAN_NONE } wifi_second_chan_t;
typedef struct { int unused; } wifi_init_config_t;
typedef enum { WIFI_OFFCHAN_TX_CANCEL, WIFI_OFFCHAN_TX_REQ } wifi_action_tx_t;
typedef int (*wifi_action_rx_cb_t)(uint8_t *, uint8_t *, size_t, uint8_t);
/* ESP request alignment is 4 on the 32-bit targets, even on this 64-bit host. */
#pragma pack(push, 4)
typedef struct {
    wifi_interface_t ifx; uint8_t dest_mac[6]; wifi_action_tx_t type;
    uint8_t channel; wifi_second_chan_t sec_channel; uint32_t wait_time_ms;
    bool no_ack; wifi_action_rx_cb_t rx_cb; uint8_t op_id; uint8_t bssid[6];
    uint32_t data_len; uint8_t data[0];
} wifi_action_tx_req_t;
#pragma pack(pop)
esp_err_t esp_wifi_action_tx_req(wifi_action_tx_req_t *request);
