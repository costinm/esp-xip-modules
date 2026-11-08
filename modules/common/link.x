/* Position independent module (RISC-V): flash references move with the code,
 * linked after the 64-byte header. Its allocator cell uses an absolute RAM
 * address supplied by the packer, independent of the flash mapping. */
ENTRY(xip_handle)

SECTIONS {
  . = MODULE_VMA;
  .text : ALIGN(4) { *(.literal .literal.*) *(.text .text.*) . = ALIGN(4); }
  .rodata : ALIGN(4) { *(.rodata .rodata.* .srodata .srodata.*) . = ALIGN(4); }
  /* The component table (export_module!(components = ...)): read-only, checked by tools/mkmod.py against manifest key 13. */
  .xip_comp : ALIGN(4) { KEEP(*(.xip_comp)) }
  /DISCARD/ : { *(.comment*) *(.eh_frame*) *(.note*) }
}
