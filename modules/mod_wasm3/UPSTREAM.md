# wasm3/ - the vendored WebAssembly interpreter

Upstream: <https://github.com/wasm3/wasm3>, commit `28ecb9af6d2040e474a70f7cb7f43666740141fb` (2026-09-29, "Implement Atomics").
MIT, `LICENSE.wasm3`.

## What is vendored

`source/` without what a module has no use for: the ten core files (`m3_bind`, `m3_code`, `m3_compile`, `m3_core`, `m3_env`,
`m3_exec`, `m3_function`, `m3_module`, `m3_parse`, `m3_validate`) and all headers. Not vendored: the WASI and libc APIs, the
tracer, the extensions, `extra/`, tests, platform and tool directories, the build files. `modules/mod_wasm3/mod_wasm3.c` is the
only host the interpreter has.

## Configuration (the -D list is in `tools/build_c_mod.py` and `crates/wasm3-c/build.rs`, kept in step by hand)

Optional proposals, the tracer, the gas meter, snapshots and threads are off; `DEBUG` must stay *undefined* (any value, `0` too, turns
on the asserts, which print through stdio and abort). `d_m3CodePageAlignSize=1024` (default 32 KiB) keeps a code page inside a
module heap. Xtensa builds add `M3_HAS_TAIL_CALL=0`: that gcc cannot sibling-call, and wasm3's `musttail` is then an error.

## Patch to upstream

1. `m3_core.c`, `m3_Malloc_Impl`/`m3_Free_Impl`/`m3_Realloc_Impl`: with `-DXIP_WASM3_ALLOC` they call `xip_w3_alloc`/`free`/`realloc`
   (port/xip_wasm3.c), which are the host's `mem_alloc`/`mem_free`: every byte the interpreter and the app hold is a tracked block.

## Size

Linked as a module image, no `.data`, no `.bss`: esp32c6 90 KB of code, esp32s3 87 KB, esp32 87 KB (wasm3 with the libc of
the `native` table, softfloat helpers from libgcc included).
