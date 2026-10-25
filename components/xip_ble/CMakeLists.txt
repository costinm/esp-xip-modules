# The `ble` component (include/xip.h xip_ble): the NimBLE mechanics of the DMesh companion link, behind a table the runtime offers through
# get_component("ble"). The sources always build; with CONFIG_XIP_BLE=n (or no NimBLE) they are stubs and pull in no Bluetooth code.
# The NimBLE configuration itself (roles, connections, bonds, buffers) is the bt component's: see profiles/ and README.md.
idf_component_register(
    SRCS "src/xip_ble.c"
    INCLUDE_DIRS "include"
    REQUIRES bt xip_runtime
    PRIV_REQUIRES freertos log esp_timer)
