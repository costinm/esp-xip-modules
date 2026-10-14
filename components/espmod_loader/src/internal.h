/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/semphr.h"
#include "esp_partition.h"
#include "espmod_check.h"
#include "espmod_loader.h"
#include "espmod_host.h"

typedef struct { uint32_t code_vaddr, data_vaddr, mapped; } fixed_map_t;

typedef struct {
    espmod_instance_t *inst;      /* NULL when not running */
#if CONFIG_IDF_TARGET_ESP32 || CONFIG_IDF_TARGET_ESP32S3
    fixed_map_t fixed;
#else
    esp_partition_mmap_handle_t mmap;
#endif
    uint8_t *request;             /* last start request, kept for a restart */
    size_t request_len;
    espmod_slot_status_t st;
    int64_t started_us;
} em_slot_state_t;

typedef struct {
    espmod_loader_config_t cfg;
    const esp_partition_t *part;
    em_slot_state_t *slots;
    SemaphoreHandle_t lock;       /* guards em_slot_state_t.inst: a stop or post must not meet the exit of the task */
    bool inited;
} em_loader_t;

extern em_loader_t em;

/* Shared by loader.c and upload.c */
espmod_slot_rules_t em_rules(int slot);
esp_err_t em_read_header(int slot, espmod_dmod_header *h);
