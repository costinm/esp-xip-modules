/* Position independent module (RISC-V): flash references move with the code,
 * linked after the 64-byte header. Its allocator cell uses an absolute RAM
 * address supplied by the packer, independent of the flash mapping. */
ENTRY(xip_handle)

SECTIONS {
  . = MODULE_VMA;
  .text : ALIGN(4) { *(.literal .literal.*) *(.text .text.*) . = ALIGN(4); }
  .rodata : ALIGN(4) { *(.rodata .rodata.* .srodata .srodata.*) . = ALIGN(4); }
  /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.note*) }
}
