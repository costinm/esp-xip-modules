idf_component_register(
    SRCS "src/espmod_host.c" "src/espmod_periph.c" "src/espmod_serial.c" "src/espmod_events.c"
    INCLUDE_DIRS "include" "../../include"
    PRIV_INCLUDE_DIRS "src"
    REQUIRES espmod_pool esp_driver_gpio esp_driver_uart esp_driver_usb_serial_jtag esp_hw_support esp_driver_spi esp_driver_i2c esp_driver_rmt esp_adc nvs_flash
             esp_timer esp_rom heap log freertos)
