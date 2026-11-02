idf_component_register(
    SRCS "src/loader.c" "src/xip_check.c" "src/xip_select.c"
    INCLUDE_DIRS "include"
    PRIV_INCLUDE_DIRS "src" "../../boot/bootloader_components/main"   # esp_wifi_loader_rtc.h: the RTC contract with Stage2
    REQUIRES xip_runtime esp_partition esp_timer
    PRIV_REQUIRES esp_hw_support esp_mm spi_flash esp_system freertos log app_update)
