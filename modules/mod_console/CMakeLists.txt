# The console module, linked into an application (a module that is not flashed: the first thing a new board needs
# is a way to type `wifi start`). Both mains use it: main/main.c and the Rust one (crates/espmod-host).
idf_component_register(
    SRCS "mod_console.c" "start.c"
    INCLUDE_DIRS "."
    PRIV_INCLUDE_DIRS "../common"
    REQUIRES espmod_host)
set_source_files_properties("mod_console.c" PROPERTIES COMPILE_DEFINITIONS ESPMOD_LINKED=1)
