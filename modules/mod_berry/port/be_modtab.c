/* SPDX-License-Identifier: Apache-2.0 */
/* The table `import` searches (be_module.c: be_module_table). Upstream keeps this file in the
 * application (default/be_modtab.c); it is ours to write because it decides what a Berry program can
 * reach. Only pure modules: see port/berry_conf.h for what is switched off and why. The list, map,
 * range, bytes, and file classes are not modules - be_loadlibs registers them when the VM starts. */
#include "berry.h"

be_extern_native_module(string);
be_extern_native_module(global);
be_extern_native_module(gc);
be_extern_native_module(undefined);

BERRY_LOCAL const bntvmodule_t* const be_module_table[] = {
#if BE_USE_STRING_MODULE
    &be_native_module(string),
#endif
#if BE_USE_GLOBAL_MODULE
    &be_native_module(global),
#endif
#if BE_USE_GC_MODULE
    &be_native_module(gc),
#endif
    &be_native_module(undefined),
    NULL /* do not remove */
};

/* const unlike upstream: a packed image is read-only flash. The VM only reads this table
 * (be_var.c: global_native_class_find), and the port lists no static classes in it anyway. */
BERRY_LOCAL const bclass_array be_class_table = {
    /* &be_native_class(my_class), */
    NULL, /* do not remove */
};
