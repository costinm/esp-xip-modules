idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES espmod_host espmod_loader espmod_wifi nvs_flash esp_timer)

target_include_directories(${COMPONENT_LIB} PRIVATE "${ESPMOD_PRODUCT_INCLUDE}")
