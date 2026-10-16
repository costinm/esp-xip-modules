/* SPDX-License-Identifier: Apache-2.0 */
/* The smallest module: log a line and echo the start request back as an event, then finish. Copy this to start a
 * new module (docs/writing-modules.md). */
#include "xip_module.h"

XIP_HANDLER(hello)(const xip_host *host, void *state, const xip_call *call)
{
    (void)state;
    if (call->component != XIP_HOST_COMPONENT || call->method != XIP_HOST_START) return XIP_RET_CONTINUE;
    xip_log(host, "hello from mod_hello");
    if (call->len) host->emit(host->user, 1, XIP_VALUE_TEXT, call->data, call->len);
    return XIP_RET_DONE;
}
