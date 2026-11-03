/* Activity classification runs after decoding, independently of the bearer. */
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include "xip_envelope.h"

int main(void)
{
    const uint8_t directed[] = {0xa3,1,0x18,46,2,1,5,0xa0};
    const uint8_t answer[] = {0xa3,1,0x18,46,2,1,6,0xa0};
    const uint8_t truncated[] = {0xa3,1,0x18,46,2,1,5,0xa1,1};
    const uint8_t trailing[] = {0xa3,1,0x18,46,2,1,5,0xa0,0};
    const uint8_t invalid_fields[] = {0xa3,1,0x18,46,2,1,5,0x01};
    const uint8_t invalid_answer[] = {0xa3,1,0x18,46,2,1,6,0xa1};
    assert(xip_envelope_activity(directed,sizeof directed,true));
    assert(xip_envelope_activity(answer,sizeof answer,true));
    assert(!xip_envelope_activity(directed,sizeof directed,false));
    assert(!xip_envelope_activity(answer,sizeof answer,false));
    assert(!xip_envelope_activity(truncated,sizeof truncated,true));
    assert(!xip_envelope_activity(trailing,sizeof trailing,true));
    assert(!xip_envelope_activity(invalid_fields,sizeof invalid_fields,true));
    assert(!xip_envelope_activity(invalid_answer,sizeof invalid_answer,true));
    puts("activity CBOR admission: ok");
    return 0;
}
