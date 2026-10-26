/* Local byte-triggered wake. RX requests it; only the serialized worker ACKs
 * after active module roles have been restored. No complete PPP RX is needed. */
#include <stdatomic.h>
#include "xip_runtime.h"

static atomic_bool pending;

void xip_serial_note_wake(void)
{
    /* A local wake also re-enables application RX; otherwise serial-off
     * could ACK and then silently discard the host's real command. */
    xip_serial_receive_enabled(true);
    atomic_store(&pending, true);
    xip_power_activate(5000);
}

void xip_serial_ack_awake(void)
{
    if (!atomic_exchange(&pending, false)) return;
    const uint8_t ack = 1;
    if (xip_serial_write_frame(0xf8, &ack, 1) < 0) atomic_store(&pending, true);
}
