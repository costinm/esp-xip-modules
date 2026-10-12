idf_component_register(
    SRCS "src/install.c"
    INCLUDE_DIRS "include"
    REQUIRES espmod_loader esp_wifi_loader
    PRIV_REQUIRES log)
