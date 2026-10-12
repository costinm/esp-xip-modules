#pragma once
typedef void *esp_partition_mmap_handle_t;
#include <stddef.h>
#include <stdint.h>
#include "esp_err.h"
typedef struct { uint32_t address, size; } esp_partition_t;
esp_err_t esp_partition_read(const esp_partition_t *p, size_t off, void *dst, size_t n);
esp_err_t esp_partition_write(const esp_partition_t *p, size_t off, const void *src, size_t n);
esp_err_t esp_partition_erase_range(const esp_partition_t *p, size_t off, size_t n);
