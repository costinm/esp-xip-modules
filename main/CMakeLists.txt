idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES xip_runtime xip_loader nvs_flash esp_timer)

