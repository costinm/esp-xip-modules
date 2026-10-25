/* Generic action egress for modules. Protocol construction belongs to the
 * module; native request framing uses its exclusive packet headroom. */
#include <stddef.h>
#include <string.h>
#include "esp_wifi.h"
#include "xip_wifi.h"

static int action_link = -1;
static bool active;
static int action_tx(void *ctx, uint64_t destination, const xip_link_packet *pkt)
{
    (void)ctx; (void)destination;
    const size_t header = offsetof(wifi_action_tx_req_t, data);
    if (!active) return XIP_ERR_UNSUPPORTED;
    if (!pkt || pkt->len < 30 || pkt->data[0] != 0xd0 || pkt->headroom + 24 < header ||
        xip_pool_refs(pkt->pool, pkt->slot) != 1) return XIP_ERR_ARG;
    /* The request overlaps only the caller's 802.11 header, never its action
     * body. Save that small header so BUSY restores the exact prepared frame. */
    uint8_t ieee[24]; memcpy(ieee, pkt->data, sizeof(ieee));
    wifi_action_tx_req_t *r = (void *)(pkt->data + 24 - header);
    if ((uintptr_t)r % _Alignof(wifi_action_tx_req_t)) return XIP_ERR_ARG;
    uint8_t channel; wifi_second_chan_t secondary;
    if (!xip_wifi_current_channel(&channel, &secondary)) return XIP_ERR_IO;
    memset(r, 0, header);
    r->ifx = xip_wifi_ap_only_active() ? WIFI_IF_AP : WIFI_IF_STA;
    memcpy(r->dest_mac, ieee + 4, 6); memcpy(r->bssid, ieee + 16, 6);
    r->channel = channel; r->sec_channel = secondary;
    r->type = WIFI_OFFCHAN_TX_REQ; r->wait_time_ms = 10;
    /* NAN discovery-group actions require the SDK's acknowledged action
     * path, matching native NAN; no_ack=true suppresses their transmission. */
    r->no_ack = false; r->data_len = pkt->len - 24;
    /* IDF lacks tx_by_ref and copies internally. The lease stays ours until
     * return; there is no adapter-owned payload allocation or TX queue. */
    int rc = xip_wifi_action_tx_req(r);
    memcpy(pkt->data, ieee, sizeof(ieee));
    return rc == ESP_OK ? XIP_LINK_TX_DONE : rc == ESP_ERR_NO_MEM ? XIP_ERR_BUSY : XIP_ERR_IO;
}
int xip_link_wifi_action_start(void)
{
    if (action_link < 0) action_link = xip_link_native_register("wifi-action", XIP_PKT_MAX, 0, action_tx, NULL);
    if (action_link < 0) return action_link;
    active = true;
    return xip_link_native_set_state(action_link, XIP_LINK_UP, 1000);
}

void xip_link_wifi_action_stop(void)
{
    active = false;
    if (action_link >= 0) (void)xip_link_native_set_state(action_link, XIP_LINK_DOWN, 0);
}
