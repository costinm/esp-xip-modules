/* Experimental raw-IP link. Native mechanisms only; tunnel/auth policy is module-owned. */
#pragma once
#include "xip.h"
int xip_ip_open(uint32_t owner, const xip_ip_config *config, uint32_t *link);
int xip_ip_set_up(uint32_t owner, uint32_t link, uint32_t up);
int xip_ip_close(uint32_t owner, uint32_t link);
void xip_ip_release(uint32_t owner);
void xip_ip_suspend_all(void);
/* Aggregates virtual connectivity with actual Wi-Fi connectivity. */
void xip_host_ip_connected(bool connected);
