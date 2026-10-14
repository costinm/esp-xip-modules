cmake_minimum_required(VERSION 3.16)

# esp-xip-modules `main`: a standalone application that hosts modules. Build with scripts/build.sh.
set(EXTRA_COMPONENT_DIRS "${CMAKE_CURRENT_LIST_DIR}/components")
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
idf_build_set_property(MINIMAL_BUILD ON)
project(esp-xip-modules)

# C sample only: compile SDK networking/peripheral diagnostics out. This file
# is not used by the Rust host's independent ESP-IDF project. Keep the host
# and module/application logging available.
set(quiet_network_components lwip esp_netif esp_adc esp_ringbuf)
idf_build_get_property(quiet_components BUILD_COMPONENTS)
foreach(component IN LISTS quiet_components)
    if(component MATCHES "^esp_driver_" OR component MATCHES "^esp_hal_" OR
       component IN_LIST quiet_network_components)
        idf_component_get_property(component_lib ${component} COMPONENT_LIB)
        get_target_property(component_type ${component_lib} TYPE)
        # Header-only components have no implementation to quiet. Do not
        # propagate the local level into the host through INTERFACE flags.
        if(NOT component_type STREQUAL "INTERFACE_LIBRARY")
            target_compile_definitions(${component_lib} PRIVATE LOG_LOCAL_LEVEL=0)
        endif()
    endif()
endforeach()
