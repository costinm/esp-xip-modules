/* SPDX-License-Identifier: Apache-2.0 */
/* Stand-ins for the parts of the loader that need an ESP: a flash that obeys NOR rules, a slot table,
 * the loader's module versions in memory, and stop/start that only record. install.c is compiled unchanged. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "esp_partition.h"
#include "internal.h"
#include "fake_loader.h"
#include "esp_wifi_loader.h"

em_loader_t em;
uint8_t fake_flash[FAKE_FLASH_SIZE];
int fake_flash_violations;
int fake_stops, fake_starts, fake_running_slot = -1;
int fake_stop_result;
static esp_partition_t part = {.address = 0x3c0000, .size = FAKE_FLASH_SIZE};
static uint32_t versions[8];
static em_slot_state_t states[4];
static const espmod_slot_t slots[] = {
    {"hello", 1, 0x00000, 0x10000}, {"test", 2, 0x10000, 0x10000}, {"lora", 43, 0x20000, 0x10000}, {"hw", 45, 0x30000, 0x10000}};

void fake_reset(void)
{
    memset(fake_flash, 0xff, sizeof(fake_flash));
    memset(versions, 0, sizeof(versions));
    memset(states, 0, sizeof(states));
    fake_flash_violations = fake_stops = fake_starts = 0;
    fake_running_slot = -1;
    fake_stop_result = 0;
    em.cfg = (espmod_loader_config_t){.slots = slots, .slot_count = 4, .partition = "data"};
    em.part = &part;
    em.slots = states;
    em.inited = true;
}

esp_err_t esp_partition_read(const esp_partition_t *p, size_t off, void *dst, size_t n)
{
    (void)p;
    if (off + n > FAKE_FLASH_SIZE) return ESP_FAIL;
    memcpy(dst, fake_flash + off, n);
    return ESP_OK;
}

esp_err_t esp_partition_write(const esp_partition_t *p, size_t off, const void *src, size_t n)
{
    (void)p;
    if (off + n > FAKE_FLASH_SIZE) return ESP_FAIL;
    const uint8_t *s = src;
    for (size_t i = 0; i < n; i++) {
        if (s[i] & ~fake_flash[off + i]) fake_flash_violations++;   /* NOR flash cannot set a bit without an erase */
        fake_flash[off + i] &= s[i];
    }
    return ESP_OK;
}

esp_err_t esp_partition_erase_range(const esp_partition_t *p, size_t off, size_t n)
{
    (void)p;
    if (off % 4096 || n % 4096 || off + n > FAKE_FLASH_SIZE) { fake_flash_violations++; return ESP_FAIL; }
    memset(fake_flash + off, 0xff, n);
    return ESP_OK;
}

size_t espmod_loader_slot_count(void) { return 4; }
const espmod_slot_t *espmod_loader_slot(int slot) { return slot >= 0 && slot < 4 ? &slots[slot] : NULL; }

int espmod_loader_find(const char *name)
{
    for (size_t i = 0; i < 4; i++) if (!strcmp(slots[i].name, name)) return (int)i;
    return -1;
}
int espmod_loader_stop(int slot, uint32_t timeout_ms)
{
    (void)timeout_ms;
    fake_stops++;
    if (fake_stop_result) return fake_stop_result;
    if (fake_running_slot == slot) { states[slot].st.running = false; }
    return 0;
}
int espmod_loader_start(int slot, const uint8_t *request, size_t len)
{
    (void)request; (void)len;
    fake_starts++;
    states[slot].st.running = true;
    return 0;
}

espmod_slot_rules_t em_rules(int slot)
{
    return (espmod_slot_rules_t){.tag = slots[slot].tag, .slot_offset = slots[slot].offset, .slot_size = slots[slot].size,
                                 .host_features = 0xff, .max_stack_bytes = 65536};
}
esp_err_t em_read_header(int slot, espmod_dmod_header *h) { return esp_partition_read(&part, slots[slot].offset, h, sizeof(*h)); }
/* esp-wifi-loader's state log, as far as modules use it */
bool esp_wifi_loader_module_minver(unsigned slot, uint32_t *v) { if (slot >= 8 || !versions[slot]) return false; *v = versions[slot]; return true; }
int esp_wifi_loader_set_module_minver(unsigned slot, uint32_t v) { versions[slot] = v; return 0; }
esp_err_t esp_wifi_loader_init(void) { return ESP_OK; }
void esp_wifi_loader_set_module_sink(const esp_wifi_loader_module_sink_t *sink) { (void)sink; }
uint32_t fake_version(int slot) { return versions[slot]; }
void fake_mark_running(int slot) { fake_running_slot = slot; states[slot].st.running = true; states[slot].request = malloc(3); memcpy(states[slot].request, "abc", 3); states[slot].request_len = 3; }
