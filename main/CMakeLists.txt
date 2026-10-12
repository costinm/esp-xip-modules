idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES espmod_host espmod_loader espmod_install mod_console esp_wifi_loader nvs_flash esp_timer)
