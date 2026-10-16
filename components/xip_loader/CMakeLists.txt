idf_component_register(
    SRCS "src/loader.c" "src/xip_check.c"
    INCLUDE_DIRS "include"
    PRIV_INCLUDE_DIRS "src"
    REQUIRES xip_runtime esp_partition esp_timer
    PRIV_REQUIRES esp_hw_support esp_mm spi_flash esp_system freertos log)
