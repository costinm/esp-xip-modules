/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "esp_err.h"
/* After espmod_host_init(): create the console module and start it. It opens the serial port itself. */
esp_err_t mod_console_start(void);
