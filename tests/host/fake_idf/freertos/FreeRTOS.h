#pragma once
#include <stdint.h>
#include <stddef.h>
typedef void *TaskHandle_t;
typedef void *QueueHandle_t;
typedef uint32_t TickType_t;
typedef int BaseType_t;
#define pdMS_TO_TICKS(x) (x)
#define pdTRUE 1
#define pdFALSE 0
#define portMAX_DELAY 0xffffffffu
