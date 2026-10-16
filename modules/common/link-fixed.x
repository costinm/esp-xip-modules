/* Fixed-address module (Xtensa): the loader maps the flash pages at exactly MODULE_VMA - 64 for code
 * and at MODULE_DATA_VMA - (text size) for read-only data. Literals and strings are ordinary data loads,
 * which use the data-bus alias of the same pages; the load address stays contiguous so objcopy
 * produces one flash image. tools/mkmod.py links twice to learn the size of .text. */
ENTRY(xip_handle)

SECTIONS {
  . = MODULE_VMA;
  .text : ALIGN(4) { *(.literal .literal.*) *(.text .text.*) . = ALIGN(4); }
  .rodata MODULE_DATA_VMA : AT(LOADADDR(.text) + SIZEOF(.text)) ALIGN(4) {
    *(.rodata .rodata.*) . = ALIGN(4);
  }
  /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.note*) }
}
