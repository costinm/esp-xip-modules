#pragma once
#include "FreeRTOS.h"
/* Host tests run a "task" to completion on the calling thread. */
#define pdPASS 1
typedef void (*TaskFunction_t)(void *);
static inline int xTaskCreate(TaskFunction_t f, const char *n, uint32_t s, void *a, int p, TaskHandle_t *h)
{ (void)n; (void)s; (void)p; (void)h; f(a); return pdPASS; }
static inline void vTaskDelay(TickType_t t) { (void)t; }
static inline void vTaskDelete(TaskHandle_t t) { (void)t; }
