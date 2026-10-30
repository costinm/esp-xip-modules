/* SPDX-License-Identifier: Apache-2.0 */
/* Berry configuration for mod_berry. berry.h includes "berry_conf.h", so this file - first on the
 * include path - configures the vendored interpreter (upstream's reference is default/berry_conf.h).
 * The choices are for a resident module: no operating system, no filesystem, no stdio, no libm, and
 * every allocation through the host's managed heap. Values follow the upstream defaults except where
 * a comment says why. */
#ifndef BERRY_CONF_H
#define BERRY_CONF_H

#include <stddef.h>

/* The allocator and the halt hooks. be_mem.c maps malloc/free/realloc onto these names and be_exec.c
 * maps abort/exit, so every block Berry holds is a host `mem_alloc` block: the runtime's balance is
 * exact and a VM that stops without freeing itself fails the test (ERR_LEAK) instead of eating the
 * device heap slowly. The implementations are in port/xip_berry.c. */
#define BE_EXPLICIT_MALLOC  xip_be_malloc
#define BE_EXPLICIT_FREE    xip_be_free
#define BE_EXPLICIT_REALLOC xip_be_realloc
#define BE_EXPLICIT_ABORT   xip_be_abort
#define BE_EXPLICIT_EXIT    xip_be_exit
void *xip_be_malloc(size_t size);
void xip_be_free(void *ptr);
void *xip_be_realloc(void *ptr, size_t size);
void xip_be_abort(void);
void xip_be_exit(int status);

#define BE_DEBUG                        0
#define BE_INTGER_TYPE                  2   /* long long: one integer type on every target */
#define BE_USE_SINGLE_FLOAT             0
#define BE_USE_PRECOMPILED_OBJECT       1   /* the const-object tables generated from the sources' annotations
                                             * (modules/mod_berry/tools/coc, run once; generate/ is committed).
                                             * This is upstream's default and the only path its tests exercise:
                                             * the non-precompiled path in this snapshot has rot (module
                                             * attribute tables miss members and lose varargs - see UPSTREAM.md).
                                             * `import string` etc. resolve through port/be_modtab.c. */
#define BE_DEBUG_SOURCE_FILE            1   /* the compiled source's name, so a traceback says `request:1:` */
#define BE_DEBUG_RUNTIME_INFO           1   /* line numbers in the traceback of a syntax/runtime error */
#define BE_DEBUG_VAR_INFO               0
#define BE_USE_PERF_COUNTERS            1   /* vm counters, and the observability hook that lets a later
                                             * release interrupt an endless script */
#define BE_VM_OBSERVABILITY_SAMPLING    16   /* upstream 20 (~1M instructions between looks): check the hook
                                              * every ~65k instructions so a runaway script is noticed sooner */
#define BE_STACK_TOTAL_MAX              2000 /* the upstream 20000 is for desktop RAM; a script that deep
                                              * gets a stack-overflow error instead of a big heap peak */
#define BE_STACK_FREE_MIN               10
#define BE_STACK_START                  50
#define BE_CONST_SEARCH_SIZE            50
#define BE_BYTES_MAX_SIZE               (8*1024) /* upstream allows 32 KiB per bytes() object; that is a big
                                                  * bite of a module's heap quota */
#define BE_USE_STR_HASH_CACHE           0
#define BE_USE_FILE_SYSTEM              0   /* no directories, no cwd */
#define BE_USE_SCRIPT_COMPILER          1
#define BE_USE_BYTECODE_SAVER           0   /* file I/O */
#define BE_USE_BYTECODE_LOADER          0   /* file I/O */
#define BE_USE_SHARED_LIB               0   /* dlopen */
#define BE_USE_OVERLOAD_HASH            1
#define BE_MAX_PARSER_DEPTH             25  /* upstream's own limit for an 8 KiB stack */
#define BE_USE_DEBUG_HOOK               0
#define BE_USE_DEBUG_GC                 0
#define BE_USE_DEBUG_STACK              0
#define BE_USE_MEM_ALIGNED              0   /* the be_mem.c split to a 32-bit RAM arena (berry_malloc32) */

/* Built-in modules: pure Berry-side libraries only. `math` needs libm, `time`/`os` need the operating
 * system, `json`/`solidify` pull in big formatters, and their sources are not even compiled - see
 * scripts/build-host-c-modules.sh. */
#define BE_USE_STRING_MODULE            1
#define BE_USE_JSON_MODULE              0
#define BE_USE_MATH_MODULE              0
#define BE_USE_TIME_MODULE              0
#define BE_USE_OS_MODULE                0
#define BE_USE_GLOBAL_MODULE            1
#define BE_USE_SYS_MODULE               0
#define BE_USE_DEBUG_MODULE             0
#define BE_USE_GC_MODULE                1
#define BE_USE_SOLIDIFY_MODULE          0
#define BE_USE_INTROSPECT_MODULE        0
#define BE_USE_STRICT_MODULE            0

#endif
