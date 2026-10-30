# berry/ - the vendored Berry interpreter

Upstream: <https://github.com/berry-lang/berry>, commit
`65fce31732a9bdb91cb42cfc86e2b40f16c57514` (2026-10-11, the 1.1.0 development line - "Keep a long
string whole in string.format when only a width is given (#557)").

## What is vendored

- `berry/` - upstream `src/` complete except `be_repl.c` and `be_repl.h`: a module has no terminal,
  and port/xip_berry.c owns `be_writebuffer` and `be_readstring` instead.
- `tools/coc/` - the generator for the const-object tables (pure Python, nothing else of `tools/`).
- `generate/` - the committed output of one run of that generator. The interpreter is built with
  `BE_USE_PRECOMPILED_OBJECT 1`, so the tables are part of the source set; sources include them as
  `../generate/be_fixed_*.h`, which is why `generate/` sits next to `berry/`. Regenerate from
  `modules/mod_berry` after touching an annotation or the port conf:

      python3 tools/coc/coc -o generate berry -c port/berry_conf.h

## What is not vendored

`default/` (upstream's reference port: the main program, REPL glue, libc platform layer, its
`be_modtab.c`), `tests/`, `examples/`, `modules/`, the build files.

## Patches to upstream

Each site carries a comment. 1-3 fix bit rot on the `!BE_USE_PRECOMPILED_OBJECT` path, which
upstream's own default build never compiles; 4 is a packing constraint. Nothing else is touched.

1. `berry.h`, `be_native_module_attr_table`: `bntvmodobj` -> `bntvmodobj_t` - the first is not a
   type name this snapshot has.
2. `berry.h`, `be_define_native_module`: `bntvmodule` -> `bntvmodule_t`, and the `.init` member is
   dropped - the struct has no init field, and every upstream caller passes NULL anyway.
3. `be_object.c`, `be_vtype2bstring`: the dead `#else` branch called `be_newstr(vm, ...)` in a
   function that has no `vm` - it cannot compile. Replaced with `return NULL`; the one caller on
   that path handles a null name.
4. `be_var.c`: `be_class_table` (defined by the port's `be_modtab.c`) is `const` here and there -
   upstream's declaration wants a writable global, but a packed image is read-only flash, and the
   VM only reads this table (`global_native_class_find`).

Why the precompiled tables are not optional: with `BE_USE_PRECOMPILED_OBJECT 0` this snapshot loses
the argument information in the annotated attribute tables (varargs, `string.format`'s args) and
drops members outright (`string.upper`). With them the interpreter behaves like upstream's.

## Port deviations (port/berry_conf.h, against upstream's default conf)

- The allocator, abort and exit go to the host (`BE_EXPLICIT_*` -> `xip_be_*` in port/xip_berry.c):
  every byte the interpreter holds is a tracked host `mem_alloc` block, so the runtime's balance is
  exact and a leaking VM fails a test instead of draining the device heap.
- Libraries switched off: json, math, time, os, sys, solidify, introspect, strict, debug; and the
  file system, bytecode saver/loader, and shared-library loading. The build script compiles no
  source of a switched-off library (its skip list matches), and port/be_modtab.c offers only
  `string`, `global`, `gc`, `undefined` to `import`. The `file` class is a different case: the VM
  registers it at start whether or not anything can be opened, so `be_filelib.c` is built and the
  port's `be_f*` stubs make every open fail cleanly.
- Sizes for a microcontroller: `BE_STACK_TOTAL_MAX` 2000 (upstream's 20000 is desktop RAM),
  `BE_BYTES_MAX_SIZE` 8 KiB (upstream 32), `BE_MAX_PARSER_DEPTH` 25, `BE_USE_STR_HASH_CACHE` 0,
  `BE_USE_MEM_ALIGNED` 0.
- Debug info: `BE_DEBUG_SOURCE_FILE` and `BE_DEBUG_RUNTIME_INFO` on, so a traceback reads
  `request:1: in function `main``; `BE_DEBUG_VAR_INFO` off. `BE_USE_PERF_COUNTERS` is on with
  `BE_VM_OBSERVABILITY_SAMPLING` 16 - a hook for a later wall-clock guard against endless scripts.
- One number model everywhere: `BE_INTGER_TYPE` 2 (long long), `BE_USE_SINGLE_FLOAT` 0. Note for
  the device phase: the core's `%` on reals calls `fmod`, still an unresolved libc dependency.
