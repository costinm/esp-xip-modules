# Wasm modules: changes and differences to port

`xip-wasm` is the module loader of the runtimes; the C runtime (`components/xip_runtime`) and `xip-runtime-host` stay.
This list tracks what changed in the host runtime for wasm modules and what the C runtime still has to take over, and what the
future mmap-based loader (XIP modules mapped from flash/files) shares. Update it with every change.

## Changed in `xip-runtime-host`

| Topic | Change | C runtime |
|---|---|---|
| Loader | `Loaded::open` takes `NAME.wasm` (+ `NAME.json`), one file per slot (`NAME.sN.wasm`); `libloading`/`.so` removed. `Loaded::with_handler` keeps the metadata and links a native handler (tests). | A flash image is mapped at its slot: same slot arithmetic. |
| Handler | `Inst.handler` is `Native(fn)` or `Wasm { space, slot }`; every call goes through `Inst::run`. | none |
| Shared memory | The first wasm module creates a fixed-size `WasmSpace` (base never moves). | none |
| Packet pool | `Pool::attach_region`: the storage of all slots is one block inside the space (idle slot allocation/trim stop); only while no packet is in use. The 16/16 handle is unchanged. | Pool storage could be a region too; not needed. |
| `mem_alloc` | A wasm instance's `Mem` allocates first fit from its own arena in the space (`Mem::with_arena`), quota and balance as before. | none |
| Slots | A module takes the first slot no running wasm instance holds; stopping gives it back (state block and heap are reused and cleared, the slot memory is zeroed when it loads). At most 8 running wasm modules. | Slots are the 64 KiB MMU pages. |
| State | A wasm module's state block is at most 16 KiB. | none |

## ABI in wasm32 (`xip-abi`, `xip-wasm`)

| Topic | Wasm | Native |
|---|---|---|
| Host calls | Named imports of module `xip`; the bridge calls the runtime's `Host` table with native pointers into the shared memory and temporaries for `usize`/`u16` out parameters. | Function-pointer table. |
| `hw` component | Named imports (`gpio_*`, `irq_*`, `spi_*`, `i2c_transfer`, `adc_read`, `rgbled_write`), bridged to the runtime's `Hardware` table. | `get_component("hw")` table. |
| Other component tables | `wifi`, `wifi-tuning`, `link`, `power`, `nimble`: `get_component` returns a static table in the module (`xip-abi/src/vt.rs`) whose functions are shims for the import `xip.vcall(component, method, a0..a5)`; the host (`xip-wasm/src/vtable.rs`) checks pointer arguments and calls the native table (structs are pointer-free, so layouts match). `crypto`, `identity` (packets in and out). | `get_component(name)` vtables with `ctx` first. |
| Tables a wasm module hosts (`ble`) | Entries are indexes in the module's exported function table (`--export-table`); the `hosted.rs` thunks call `WasmSpace::call_table`, copying struct arguments through the scratch area as the per-method `signature()` in `hosted.rs` says (only `ble` has one). | Function pointers in module memory. |
| Interpreter | `wasmi` 2.0 with `validate` and `portable-dispatch`: the default tail-call dispatch needs optimized code and overflows the stack of an unoptimized (debug) build. | n/a |
| Unsupported call | `XIP_WASM_TRACE=1` prints the name (the module calls `xip.note_unsupported`). | n/a |

## To do

1. Crypto: `psa` is retired. Modules use the `crypto` and `identity` tables (`xip-abi/src/crypto.rs`, `identity.rs`): calls on packet ranges, handles for
   multipart hashes, no native contexts, no crypto crates in modules (`xip_abi::CryptoClient` does the packet handling). The host implements them in
   `xip-runtime-host/src/crypto.rs` over `crates/xip-psa` (pure Rust; the identity persists in `XIP_STATE_DIR/identity.p256` or `set_identity_dir`); both are in the
   generated schema, so wasm modules reach them like any table. The C runtime has `xip_crypto.c` (over `psa_*`) and packet-based `xip_identity.c`: **written
   but not built** (no ESP-IDF here); `include/xip_psa.h` and `xip_psa.c` are removed. Open: AEAD for QUIC protection (runtime-internal), `stream` bridge test,
   `signature()` for the other hosted tables (`lora`, `nan`, `system`).
2. Replace the pointer-taking methods by pointer-free calls (`modules/ABI-POINTERS.md`), after which the bridges need no pointer checks.
3. `sim` features and builds for the other modules; the dynamic `(component, method)` call and `get_component` by name for hosted components.
4. Port to the C runtime, optional (it trusts native modules; the check finds bugs, mostly on the host): the owner check on packets (only the last receiver writes or sends), `pool_max`/variable growth is not needed.
5. Optional debug check that a module touches only its slot, heap and held packets.

6. Host classifier and QUIC link (the UDP link `udp` on `XIP_QUIC_PORT` and the Rust classifier are in; `dmesh-cli console udp://...` works against `xip-host`; `mod_quic` runs as wasm: the host `quic` table has `attach`/`detach`, and `dmesh-cli ... quic.status` works; `mod_disc` runs too (`dmesh-cli ... disc.announce` returns its signed record); a QUIC handshake/stream test and the `stream` bridge are open): the host must run `mod_quic`, `mod_disc`, `mod_lora` (with `mod_con`, later a `mod_hwproxy` for SPI) as on the device, so it
   needs the C runtime's classifier (UDP 3339 default port, real QUIC short/long packets vs the temporary `0x444d0002` envelope packets, relay
   and other untrusted exceptions). `dmesh-cli` reaches the host through it. Port 5227 and NAN/Aware are discovery/sleep specifics, not part of this.

7. Private memory per module (prototype: `xip-wasm/src/arena.rs`, `tests/arena.rs`; Linux/Android only): each instance gets its own memory, anonymous
   private memory from address 0 and the pool (a `memfd`, `MAP_SHARED`) above it at the same address in every instance, handed to wasmi with
   `Memory::new_static`. Wasmi zero-fills a static buffer, so the range is given to wasmi first and the pool is mapped over its tail afterwards.
   Shown: two instances see the same pool bytes, the private parts differ, an access past the end traps. Not integrated: it removes the slot
   arithmetic (`slot_base`, `.sN.wasm`, one build per module linked at 0) but every host-pointer/address translation (`Ctx::ptr`, `addr`, state
   blocks, `mem_alloc` arenas, the scratch area) becomes per instance, and a private scratch/state area per instance replaces the shared ones.

8. Nothing that crosses the boundary is larger than a packet (1500 bytes): the output of a call that can be larger goes to a `stream` (flow control, or
   dropped when nobody reads), not to a buffer of the caller. By signature (an `out` + `capacity`; sizes not measured yet), the candidates are
   `modules_info`, `device_info` (grows with the modules, components and packets it lists), `module_manifest`, `heap_info`/`heap_dump` (kind 6 already streams
   CBOR to PPP records), `nv` and `get_setting` values, and discovery/neighbor listings; `wifi.scan` is at most 16 entries and `flash_read` one pool slot, so they fit.
   A module's `reply` is already capped (`XIP_RESPONSE_MAX`, 1024, `ERR_SIZE`). To do: measure the largest answer of each on a full host, mark the ones over a
   packet with a `/// note=` line in the table (the generator then lists them), and give each a stream form; the bulk-bytes inputs (`hw.spi_transfer`,
   `i2c_transfer`) are the same question.

9. `native` (`xip-abi/src/native.rs`, `components/xip_runtime/src/xip_native.c`, host `xip-runtime-host/src/native.rs`): the C library of a native module as a
   `const` table of plain C functions (own signatures, no `ctx`): the ROM's or the firmware's `memcpy`, `strlen`, `strtod`, `floor`, ..., and the runtime's
   formatter (`xip_fmt.c`) as `snprintf`. No allocation for the module, nothing remembered about the caller. A C module image (no `.data`/`.bss`, no libc
   archive) calls it through word 1 of its RAM cell, which its handler fills on every call (`modules/common/xip_native_libc.h`, `.c`; `build_c_mod.py`
   passes the header with `-include`). `mod_berry` uses it (its own libc is gone, -5 KB per chip). A wasm module does not get it (its memory is not
   the runtime's); the host table has no `snprintf` (Rust cannot define a variadic function). Not in the table: libgcc's float/64-bit helpers (the
   compiler calls them directly: the ROM's `esp32*.rom.libgcc.ld` addresses could be linked in at module build time) and anything that needs the
   module's heap.
10. wasm3 (`modules/mod_wasm3`, component 51, a C module like Berry): runs a small wasm app that a request carries - not aligned to anything, no slot, gone
   when the call returns. `run {1: wasm bytes, 2: export?, 3: [int args]?, 4: memory cap?}` answers `{1: result?, 2: logged text or error?, 3: status}`.
   The app imports `xip.log` and `xip.now_us`; its memory may use the custom page size proposal (a 2 KiB memory runs in 2 KiB) and is capped per request.
   Every byte (interpreter and app) is a tracked `mem_alloc` block; the host tests (`crates/xip-runtime-host/tests/c_wasm3.rs`, via `crates/wasm3-c`) check zero
   leaks after results, traps, bad binaries and over-cap memory. The same sources link as a module image with no `.data`/`.bss`
   (esp32c6 90 KB, esp32s3 and esp32 87 KB, libc through the `native` table: the math functions of the table are its trampolines).
   Registered for esp32, esp32s3 and esp32c6 (slot 16, span 2; `XIP_DMOD_STATIC_SLOTS` is now 64). The classic ESP32 has 27 link windows since 2026-11-01 (code base 0x40250000, data 0x3F650000, under a runtime of at most 1.5 MiB), so wasm3 builds there (87,488 B) and boots on board 4170 with the module set; not yet started on the device. It has no tail calls on Xtensa (see below).
   On Xtensa the interpreter is built without tail calls (that gcc cannot sibling-call), so a long-running loop nests natively and traps at
   `d_m3MaxNativeStack` (5 KiB): small apps only there. Not done: any call from the app into a component (`xip.call` with the (component,
   method) form is the part that exercises the ABI), a measured heap need (the host test uses 512 KiB for 64 KiB pages), a run on a device.
   Compiled to wasm32 (`-mtail-call`, `d_m3CanTailCall=1`, a native stack budget below the wasm stack) it runs apps inside wasmtime, so a wasm module
   holding wasm3 looks feasible; it was not run under `xip-wasm`.

## Pointer audit and boundary rule

Goals: zero copy for bulk data on low-end devices, and isolation on the host (modules in separate processes with shared memory,
possibly over real binder on Android/Linux). `vtable`, `component` and `method` are internal: modules should not see them, so the
bridge (`vt.rs`/`vtable.rs`) is a stand-in for a call that is a **parcel**.

A call is a parcel when everything it needs is in one block: scalars, handles (packets, sockets, links), by-value C structs without
pointers, and short inline byte strings (names, keys, CBOR records). It must not depend on memory outside the block or on the
address of the block. Bulk data is never copied into a parcel: it lives in a packet, and the parcel carries the handle (plus
offset and length). Results are parcels too (status plus out-structs), never a pointer into the callee's memory.

`modules/ABI-POINTERS.md` (generated: `scripts/abi-pointers.py > modules/ABI-POINTERS.md`) lists the 76 methods with pointer
arguments. Under this rule:

* **Already parcels** (a struct or small record in or out, no pointer fields): the wifi, ble, nimble, power, relay, link-info,
  spi-config calls and the scalar out-params (`generation`, `headroom`, `socket_peer`, `adc_read`). Only the calling convention
  (pointer + size) changes, not the data.
* **Bulk bytes** that should be a packet, not a buffer: `socket_read`/`socket_write`, `spi_transfer`/`i2c_transfer` `tx`/`rx`,
  `flash_read`, `udp_send` data, `stream.feed`/`take`, `link` `route` keys are small and stay inline. `socket_write_pkt` is already the shape.
* **Position dependent, to replace**: `pkt_buf`/`pkt_span` return an address; make the packet a handle plus offset into a shared
  region that each side maps itself (address arithmetic stays local), or read/write calls for the isolated case.
  `get_component` returns a vtable pointer: replace with a lookup by name that returns a handle (the call target is then internal).
  `xip_call.data` points into the caller: it should be a packet handle (offset, length) or inline in the call parcel.
  `mem_alloc`/`mem_free` are not a boundary call: each module owns a private heap, only packets are shared.
* **Dynamic call**: `(component, method)` stays as data inside a parcel (a service handle plus a transaction code, as in binder),
  not as a table index or function pointer.

## Schema-driven calls

`crates/xip-abi/src/schema.rs` (decoder) and `schema_data.rs` (generated) are a compact static schema of the component tables (`wifi`, `wifi-tuning`, `link`, `power`, `nimble`): per
method its name and its arguments (word, struct in/out, inline string/bytes, array out), plus a `note` that flags what does not fit
the parcel model (decided case by case). The host side of the wasm bridge (`xip-wasm/src/vtable.rs`) calls a native table method from it
with one generic path, no `match` per method, and a test (`xip-abi/tests/schema.rs`) keeps it in step with the table structs. The vtable stays an
internal detail and the module-side stubs (`vt.rs`) are local. The same schema serves gateways, `mod_con` (CBOR to struct without per-method code) and
the planned proxy of the hardware (SPI, I2C, interrupts) over a stream, which is why most methods should also have a CBOR form.

The schema and the module-side stubs are generated (`mesh-api-gen --vtable-conf api/vtables.conf`, run by `scripts/gen.sh`) from the Rust table structs of
`xip-abi`, the source of truth of the ABI. It covers `wifi`, `wifi-tuning`, `link`, `power`, `nimble`, `stream`, `quic`, `hw` and the tables modules host
(`ble`, `lora`, `nan`, `system`); `hosted.rs` copies arguments from it. Flagged (`note`): `hw.i2c_transfer` has 10 arguments, more than a call
carries (needs a struct parcel); `wifi.scan*` arrays out; packet-handle ownership moves in `link.rx/tx_done`, `stream.write_pkt/take`; bulk bytes in `hw.spi_transfer/i2c_transfer`.

The generator is `mesh-api-gen` from ssh-mesh (`crates/mesh-api-gen/src/vtable.rs` there; put a checkout first with `XIP_DEV_BIN`). Rust is the
source: the table structs of `xip-abi` become the components and methods of `api/generated/tables.api.json` (`--vtable-conf api/vtables.conf`, which
only lists the structs), and the schema and the stubs are generated from that `api.json` alone (`--tables-json`). What does not fit a parcel is a
`/// note=...` line in the doc comment of the table function; a struct with a CBOR form will say `/// cbor=map` and its fields `/// tag=N`
(checked by the generator; a struct without them has no tag form and keys by name). The marker lines are not part of the prose carried on. A
build uses the checked-in `schema_data.rs` and `vt_shims.rs`; only regenerating needs the tool.

There are no component numbers. A schema is a byte string per component (`SCHEMA_WIFI`, ...; decoded by `xip_abi::schema`), a method is its index in the
table (order and version keep callers compatible). A wasm module asks `xip.component(name)` and gets a handle that is private to that runtime and may
differ between devices; the generated stubs keep it in a static. A table a module hosts is described by the module: `Hw::provide_table` passes the
schema with the table and the runtime reads it from the module's memory, so a new hosted component needs nothing in `xip-abi` or `vtables.conf` of this
tree (its own crate runs `mesh-api-gen --vtable-conf` with its own file, mode `hosted`).

Next: the CBOR side. Where a table method has an `API.md` entry (today `wifi`), its field tags (`api/generated/*.api.json`) can be joined to the
schema in this same pass, so one table drives both forms; the other tables need `API.md` files for their CBOR names and tags. Then the `hw`
component (named imports today) moves to the schema for the proxy over a stream.
