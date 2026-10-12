cmake_minimum_required(VERSION 3.16)

# esp-xip-modules `main`: a standalone application that hosts modules. Build with scripts/build.sh.
# esp-wifi-loader verifies and installs the signed packages (its component, in library mode); the checkout is
# $ESP_WIFI_LOADER_DIR (env.sh), default ../esp-wifi-loader.
if(DEFINED ENV{ESP_WIFI_LOADER_DIR})
    set(wl_dir "$ENV{ESP_WIFI_LOADER_DIR}")
else()
    set(wl_dir "${CMAKE_CURRENT_LIST_DIR}/../esp-wifi-loader")
endif()
if(NOT EXISTS "${wl_dir}/components/esp_wifi_loader/CMakeLists.txt")
    message(FATAL_ERROR "esp-wifi-loader not found at ${wl_dir}: check it out next to this repository or set ESP_WIFI_LOADER_DIR")
endif()
set(EXTRA_COMPONENT_DIRS "${CMAKE_CURRENT_LIST_DIR}/components" "${CMAKE_CURRENT_LIST_DIR}/modules/mod_console" "${wl_dir}/components")
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
idf_build_set_property(MINIMAL_BUILD ON)
project(esp-xip-modules)
