This is the 'core' XIP mapper - MMU, slots, etc.

Modules use the ABI - implemented by espmod_host. 

The loader doesn't depend on the ABI definition or implementation - it's just
a pointer that gets passed to the modules.

The MMU imposes a restriction on 64k alignment - so modules must be flashed in specific 
slots. While RiscV supports PIC, the Xtensa doesn't - so modules currently need to 
be compiled for a specific slot. Current partition uses 8 module slots, in the last part
of the 4M flash - but it depends on the user choices.

Since most modules are smaller than 64k - it is good to combine multiple features in
a module, or to use a filesystem that can use the unused blocks for storage. Or a filesystem
that can reserve continuous space at 64k offsets for the modules - all of this is out of scope
for module loader, which only cares about 64k slots and Xtensa modules compiled for the 
matching slot.

While dynamic install/upgrade of optional or custom modules is possible - and 'stop' and 
memory model in current ABI are trying to make it viable - the main use is to modularize
and allow faster independent upgrade of components in an app, and faster development
since a module can be flashed much faster - and tested on host with a fake ABI runtime. 