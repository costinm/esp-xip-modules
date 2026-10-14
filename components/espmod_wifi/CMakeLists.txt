idf_component_register(SRCS "src/wifi.c" INCLUDE_DIRS "include"
    REQUIRES espmod_host esp_wifi esp_netif esp_event nvs_flash)
