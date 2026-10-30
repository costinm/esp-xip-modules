# mod_berry - the Berry language runtime

Component `berry` (50), RAM slot 14. Runs the [Berry](https://github.com/berry-lang/berry)
scripting language in one resident VM: source comes in through requests, the answer carries back
what the interpreter printed and what the source evaluated to. The interpreter is vendored in
`berry/` (see `UPSTREAM.md`); the port is `port/`. Everything runs on the development host first;
a smoke image boots on an esp32s3 board and prints what it evaluated (see Device build).

## Behavior

- `HOST_START`: one `be_vm_new`, a log line, and the module stays resident (`XIP_RET_CONTINUE`),
  so one VM serves every later request. A non-empty start request is echoed as event 1; a start
  request beginning `run:` is also executed as Berry source, its output logged.
- `HOST_STOP`: `be_vm_delete`. Every block the interpreter held came from the host's `mem_alloc`,
  so the runtime's leak balance closes at zero.
- Request - component 50, method 1 (`eval`) - runs one piece of source:

      request  {1: text}                      Berry source
      response {1: text?, 2: text?, 3: uint}  the value the script returned (omitted when nil),
                                              what the interpreter wrote (print, errors, tracebacks)
                                              - capped at 512 bytes - and a status: 0 ok,
                                              1 compile error, 2 runtime error, 3 bad request,
                                              4 no interpreter

  A bare expression answers its value: like Berry's own REPL the source is first tried as
  `return (source)`, and when that will not compile, the source runs as written. Errors and
  tracebacks name the compiled source `request` (`request:1: in function \`main\``).

## Berry in this snapshot

- One VM serves every request, and a top-level assignment is a global variable: `count = 7` in one
  request, `count + 1` in the next answers `8`. `global().x = 7` is not the way here - the
  snapshot's `global()` returns a list that cannot be assigned to.
- The 1.1.0 development line uses the new syntax (`for i : 0 .. n`, `as`); the old `for x in y:`
  does not parse.
- `import` reaches `string`, `global`, `gc`, `undefined` (`port/be_modtab.c`); list, map, range and
  bytes are built-in classes. json, math, time, os, sys, solidify, introspect, strict and debug
  are off, `file` exists but never opens.

## Memory

The state block (1696 bytes, `modules/retained-memory.json`) holds only descriptors: the VM
pointer, the host pointer, the 512-byte output capture, and a 1152-byte scratch for building the
CBOR answer (`berry_state` in `port/xip_berry.h`, size-asserted in `mod_berry.c`). All interpreter
memory - heap, VM stack, strings - is a host `mem_alloc` block with a 16-byte head, through the
`BE_EXPLICIT_*` shims in `port/xip_berry.c`. Hooks that run without a call in hand (`be_writebuffer`,
the allocator) reach the state block through the module cell: word 0 of a 16-byte RAM cell, which
the handler installs on entry and restores on exit. A host build keeps that cell in a static; a
packed image compiles it to an absolute address (`-DXIP_CELL_ADDR`, the Rust heap's trick).

## Build and test (host)

    scripts/build-host-c-modules.sh        # target/host-modules/mod_berry.so + mod_berry.json sidecar
    cargo test --manifest-path crates/Cargo.toml -p xip-runtime-host --test c_berry

The sidecar manifest is still empty (`mkmod` with no API.md); the API text lives here and in the
module header until the module gets an API.md of its own.

## Device build

    python3 tools/build_c_mod.py modules/mod_berry --chip esp32      # target/out/esp32/modules/mod_berry.dmod
    python3 tools/build_c_mod.py modules/mod_berry --chip esp32s3    # 93,712 bytes in the two-page span

`scripts/build.sh` builds the module for the chips `modules/C-MODULES` allows. The device link is
`-nostdlib` apart from `setjmp`/`longjmp` (the toolchain's `libc.a` is the one whose register
handling matches the firmware); `port/xip_libc.c` carries everything else the interpreter asks of
libc - memory and string routines, `fmod`, and `snprintf`. The formatter core there compiles on
the host too (`xip_fmt_snprintf`), so `c_berry` tests run the very code a packed image ships.
`-DXIP_CELL_ADDR=<the slot's cell address>` compiles the cell to the fixed address of the RAM
window reserved for slot 14. A smoke image built with `--autostart 'run:...'` boots with the
loader and logs what it evaluated: `berry on esp32s3: 7*6 = 42`.

## Fixed link windows: why the const tables survive

`BE_USE_PRECOMPILED_OBJECT` makes Berry's constant tables C static initializers, and those tables
hold pointers into the image's own strings, classes and protos. They work only if the image is
mapped where it was linked, and every chip here does that: each module links at
`code_base + slot * 64 KiB` - the window slot, the index of its RAM cell - and the loader maps the
flash pages it found the image on at exactly that address (`mmu_hal_map_region`, because IDF's
esp_partition_mmap never lets the caller choose the address). Xtensa maps each module's pages
twice, into the instruction window and the data-bus alias; esp32c6 has a single linear flash
region (0x42000000, 64 KiB MMU pages) that both buses read, so its window maps once and
`data_base == code_base` there. The 1 MiB C6 window at 0x42400000 starts clear of the pages
Stage2 maps for Main or loader (at or below 0x42280000); `tests/test_tools.py` checks these
bases against `components/xip_loader/src/loader.c`.

`tools/build_c_mod.py`'s `baked_pointers` still counts in-image link-time addresses and would
reject them for a hypothetical chip without fixed windows; Rust modules keep building their
pointer tables in RAM at run time (`crates/xip-abi/src/task.rs`) and would run either way.
