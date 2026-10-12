/* SPDX-License-Identifier: Apache-2.0 */
/* The smallest module: log a line and echo the start request back as an event, then finish. Copy this to start a
 * new module (docs/writing-modules.md). */
#include "espmod_module.h"

ESPMOD_HANDLER(hello)(const espmod_host *host, void *state, const espmod_call *call)
{
    (void)state;
    if (call->component != ESPMOD_HOST_COMPONENT || call->method != ESPMOD_HOST_START) return ESPMOD_RET_CONTINUE;
    em_log(host, "hello from mod_hello");
    if (call->len) host->emit(host->user, 1, ESPMOD_VALUE_TEXT, call->data, call->len);
    return ESPMOD_RET_DONE;
}
