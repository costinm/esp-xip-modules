#pragma once
#include <stdint.h>
#define FAKE_FLASH_SIZE 0x40000
extern uint8_t fake_flash[FAKE_FLASH_SIZE];
extern int fake_flash_violations, fake_stops, fake_starts, fake_stop_result;
void fake_reset(void);
uint32_t fake_version(int slot);
void fake_mark_running(int slot);
