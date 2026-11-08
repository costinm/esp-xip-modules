/* Fixed-address module: the loader maps the flash pages at exactly MODULE_VMA - 64 for code
 * and at MODULE_DATA_VMA - (text size) for read-only data. On Xtensa those are the instruction and the
 * data-bus alias of the same pages (literals and strings are ordinary data loads); on esp32c6 one
 * linear region serves both buses, so the two windows are equal and rodata sits at its flash offset.
 * The load address stays contiguous so objcopy produces one flash image. tools/mkmod.py links twice
 * to learn the size of .text. */
ENTRY(xip_handle)

SECTIONS {
  . = MODULE_VMA;
  .text : ALIGN(4) { *(.literal .literal.*) *(.text .text.*) . = ALIGN(4); }
  .rodata MODULE_DATA_VMA : AT(LOADADDR(.text) + SIZEOF(.text)) ALIGN(4) {
    *(.rodata .rodata.* .srodata .srodata.*) . = ALIGN(4);
  }
  /* The component table (export_module!(components = ...)), read-only data after .rodata in the same flash image; tools/mkmod.py checks it
   * against manifest key 13. Its LMA follows .rodata's, so the image stays contiguous. */
  .xip_comp : AT(LOADADDR(.rodata) + SIZEOF(.rodata)) ALIGN(4) { KEEP(*(.xip_comp)) }
  /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.note*) }
}
