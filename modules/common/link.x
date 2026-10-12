/* Position independent module (RISC-V): code is linked at the end of the 64-byte image header and
 * contains no absolute addresses, so any mapping address works. */
ENTRY(espmod_handle)

SECTIONS {
  . = MODULE_VMA;
  .text : ALIGN(4) { *(.literal .literal.*) *(.text .text.*) . = ALIGN(4); }
  .rodata : ALIGN(4) { *(.rodata .rodata.* .srodata .srodata.*) . = ALIGN(4); }
  /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.note*) }
}
