idf_component_register(
    SRCS "src/xip_runtime.c" "src/xip_periph.c" "src/xip_serial.c" "src/xip_events.c" "src/xip_profile.c" "src/xip_info.c" "src/xip_psa.c" "src/xip_pool.c" "src/xip_wifi.c"
    INCLUDE_DIRS "include" "../../include"
    PRIV_INCLUDE_DIRS "src"
    REQUIRES app_update bootloader_support spi_flash esp_netif lwip esp_wifi esp_event esp_driver_gpio esp_driver_uart esp_driver_usb_serial_jtag esp_hw_support esp_driver_spi esp_driver_i2c esp_driver_rmt esp_adc nvs_flash
             esp_timer esp_rom heap log freertos mbedtls)
