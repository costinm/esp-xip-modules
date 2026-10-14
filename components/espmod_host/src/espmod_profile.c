/* Desired bearer store for the C host. Rust applications replace these hooks. */
#include "espmod_host.h"
#include "espmod_transport.h"
#include "freertos/FreeRTOS.h"
#include "nvs.h"

static portMUX_TYPE lock = portMUX_INITIALIZER_UNLOCKED;
static espmod_bearer_profile desired;
static uint32_t generation;

static int get(espmod_bearer_profile *p, uint32_t *gen)
{
    if (!p || !gen) return ESPMOD_ERR_ARG;
    if (p->size < sizeof(*p)) return ESPMOD_ERR_SIZE;
    portENTER_CRITICAL(&lock);
    *p = desired; *gen = generation;
    portEXIT_CRITICAL(&lock);
    return 0;
}
static int set(const uint8_t *request, size_t len, uint32_t *gen)
{
    if (!request || !gen) return ESPMOD_ERR_ARG;
    /* Decode outside the critical section. Retry if a concurrent writer has
     * published meanwhile; always merge into the latest snapshot. */
    for (;;) {
        espmod_bearer_profile candidate = {.size = sizeof(candidate)};
        uint32_t previous; get(&candidate, &previous);
        int rc = em_transport_apply_request(request, len, &candidate); if (rc) return rc;
        portENTER_CRITICAL(&lock);
        if (previous != generation) { portEXIT_CRITICAL(&lock); continue; }
        bool changed = !em_transport_equal(&desired, &candidate);
        if (changed) { desired = candidate; if (++generation == 0) generation = 1; }
        *gen = generation;
        portEXIT_CRITICAL(&lock);
        if (changed) espmod_host_transport_changed(*gen);
        return 0;
    }
}
void em_profile_install(espmod_host_hooks_t *h)
{
    em_transport_default(&desired);
    /* The simple STA owner uses the existing boot settings, never a profile blob. */
    if (h->wifi_start) {
        nvs_handle_t settings, secrets;
        if (nvs_open("dmesh", NVS_READONLY, &settings) == ESP_OK) {
            size_t n = sizeof(desired.ssid);
            esp_err_t rc = nvs_get_str(settings, "sta_ssid", (char *)desired.ssid, &n);
            nvs_close(settings);
            if (rc == ESP_OK && n > 1 && n <= sizeof(desired.ssid) && nvs_open("sec", NVS_READONLY, &secrets) == ESP_OK) {
                size_t m = sizeof(desired.sta_passphrase);
                rc = nvs_get_str(secrets, "sta", (char *)desired.sta_passphrase, &m); nvs_close(secrets);
                if (rc == ESP_OK && m >= 9 && m <= sizeof(desired.sta_passphrase)) {
                    desired.ssid_len = n - 1; desired.sta_passphrase_len = m - 1;
                    desired.mode = 1; desired.run_requested = 1; desired.ap = 0;
                }
            }
        }
    }
    if (!h->transport_set) {
        h->transport_set = set; h->transport_get = get;
    }
}
