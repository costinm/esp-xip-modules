#pragma once
#include "FreeRTOS.h"
typedef void *SemaphoreHandle_t;
static inline SemaphoreHandle_t xSemaphoreCreateMutex(void) { return (void *)1; }
static inline SemaphoreHandle_t xSemaphoreCreateBinary(void) { return (void *)1; }
static inline int xSemaphoreTake(SemaphoreHandle_t s, TickType_t t) { (void)s; (void)t; return pdTRUE; }
static inline int xSemaphoreGive(SemaphoreHandle_t s) { (void)s; return pdTRUE; }
