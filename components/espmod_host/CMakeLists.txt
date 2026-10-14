idf_component_register(
    SRCS "src/espmod_host.c" "src/espmod_periph.c" "src/espmod_serial.c" "src/espmod_events.c" "src/espmod_profile.c" "src/espmod_info.c" "src/espmod_psa.c"
    INCLUDE_DIRS "include" "../../include"
    PRIV_INCLUDE_DIRS "src"
    REQUIRES app_update spi_flash esp_netif lwip espmod_pool esp_driver_gpio esp_driver_uart esp_driver_usb_serial_jtag esp_hw_support esp_driver_spi esp_driver_i2c esp_driver_rmt esp_adc nvs_flash
             esp_timer esp_rom heap log freertos mbedtls)
