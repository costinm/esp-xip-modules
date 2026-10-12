/* SPDX-License-Identifier: Apache-2.0 */
/* Host side of the linked console module: create it and start it; it takes the serial port itself (serial_open). */
#include "esp_err.h"
#include "espmod_host.h"

extern int console_handle(const espmod_host *, void *, const espmod_call *);

esp_err_t mod_console_start(void)
{
    const espmod_instance_config_t cfg = {.handler = console_handle, .state_bytes = 16};
    espmod_instance_t *inst = espmod_instance_create("console", &cfg);
    if (!inst) return ESP_ERR_NO_MEM;
    return espmod_instance_start(inst, NULL, 0) ? ESP_FAIL : ESP_OK;
}
