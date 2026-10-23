idf_component_register(
    SRCS "src/xip_module_ram.c" "src/xip_runtime.c" "src/xip_lwip_io.c" "src/xip_periph.c" "src/xip_serial.c" "src/xip_ppp.c" "src/xip_events.c" "src/xip_wifi_intent.c" "src/xip_info.c" "src/xip_psa.c" "src/xip_pool.c" "src/xip_wifi.c" "src/xip_link_espnow.c" "src/xip_link_promisc.c" "src/xip_rx_filter.c" "src/xip_power.c"
    INCLUDE_DIRS "include" "../../include"
    PRIV_INCLUDE_DIRS "src"
    REQUIRES app_update bootloader_support spi_flash esp_netif lwip esp_wifi esp_event esp_driver_gpio esp_driver_uart esp_driver_usb_serial_jtag esp_hw_support esp_driver_spi esp_driver_i2c esp_driver_rmt esp_adc nvs_flash
             esp_timer esp_rom heap log freertos mbedtls)

# Read the canonical ABI constants, so linker and SDK heap reservations cannot
# silently disagree after a catalog regeneration.
set(xip_abi_header "${CMAKE_CURRENT_LIST_DIR}/../../include/xip.h")
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${xip_abi_header}")
file(STRINGS "${xip_abi_header}" xip_ram_defines
    REGEX "^#define XIP_DMOD_STATIC_(SLOT_BYTES|SLOTS|ESP32_BASE|ESP32C6_BASE|ESP32S3_BASE) [0-9]+$")
foreach(xip_define IN LISTS xip_ram_defines)
    string(REGEX MATCH "^#define ([A-Z0-9_]+) ([0-9]+)$" xip_match "${xip_define}")
    set(${CMAKE_MATCH_1} ${CMAKE_MATCH_2})
endforeach()
math(EXPR XIP_MODULE_RAM_BYTES "${XIP_DMOD_STATIC_SLOT_BYTES} * ${XIP_DMOD_STATIC_SLOTS}")

# One fixed arena is excluded from static Main sections and the SDK heap.
# This link option is exported to Rust Main as well.
if(CONFIG_IDF_TARGET_ESP32)
    set(XIP_MODULE_RAM_BASE ${XIP_DMOD_STATIC_ESP32_BASE})
    set(XIP_MODULE_RAM_IRAM_ASSERT "") # Plain DRAM: this bank has no IRAM alias.
elseif(CONFIG_IDF_TARGET_ESP32S3)
    set(XIP_MODULE_RAM_BASE ${XIP_DMOD_STATIC_ESP32S3_BASE})
    # SDK MAP_DRAM_TO_IRAM maps this bank through the fixed S3 I/D offset.
    math(EXPR xip_iram_alias "${XIP_MODULE_RAM_BASE} + 0x6f0000" OUTPUT_FORMAT HEXADECIMAL)
    set(XIP_MODULE_RAM_IRAM_ASSERT "ASSERT(_iram_end <= ${xip_iram_alias}, \"Main IRAM overlaps module RAM alias\");")
elseif(CONFIG_IDF_TARGET_ESP32C6)
    set(XIP_MODULE_RAM_BASE ${XIP_DMOD_STATIC_ESP32C6_BASE})
    set(XIP_MODULE_RAM_IRAM_ASSERT "ASSERT(_iram_end <= ${XIP_MODULE_RAM_BASE}, \"Main IRAM overlaps module RAM arena\");")
else()
    message(FATAL_ERROR "No module RAM arena for this target")
endif()
configure_file(linker/module_ram.ld.in ${CMAKE_CURRENT_BINARY_DIR}/module_ram.ld @ONLY)
target_link_options(${COMPONENT_LIB} INTERFACE "-T${CMAKE_CURRENT_BINARY_DIR}/module_ram.ld")
set_property(TARGET ${COMPONENT_LIB} APPEND PROPERTY INTERFACE_LINK_DEPENDS ${CMAKE_CURRENT_BINARY_DIR}/module_ram.ld)
