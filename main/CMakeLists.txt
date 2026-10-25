idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES xip_runtime xip_loader xip_ble nvs_flash esp_timer)

