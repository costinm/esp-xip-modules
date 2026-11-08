#!/usr/bin/env python3
"""Device build of a C module (the registry modules/C-MODULES, not a Cargo.toml): cross-compile the
module's own sources with the chip's gcc, link one read-only image, run the image checks tools/mkmod.py
runs, and pack the .dmod.

The counterpart of crates/berry-c (build.rs, which builds the same sources for the host): the same
sources and the same library skip list
(the two lists are kept in step by hand), but the freestanding rules of a packed image: no libc
archive objects beyond setjmp/longjmp, no .data/.bss, no relocations, and the module's RAM cell
passed in as -DXIP_CELL_ADDR (modules/common/xip_module.h, crates/xip-abi/src/heap.rs).

Every chip links a device image for fixed windows, so it bakes absolute addresses of its own flash
windows: correct, because the loader maps that module's pages at exactly those addresses. A PIC image
(kept for a future chip without fixed windows, no chip ships one today) moves with the flash page, so
the same baked words - C const tables of pointers, which Berry is built from - are garbage wherever
the image lands; baked_pointers() gates that. Rust modules never emit them
(crates/xip-abi/src/task.rs builds pointer tables in RAM at run time)."""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dmod  # noqa: E402
import mkmod  # noqa: E402
import build_version  # noqa: E402

ROOT = mkmod.ROOT

# Berry libraries the port's berry_conf.h switches off; crates/berry-c/build.rs keeps the same list.
SKIP = {"be_jsonlib", "be_mathlib", "be_timelib", "be_oslib", "be_solidifylib", "be_introspectlib",
        "be_strictlib", "be_debuglib", "be_bytecode"}

# wasm3 as mod_wasm3 configures it (the files of wasm3/ it compiles, the -D list); crates/wasm3-c/build.rs keeps the same.
W3_FILES = {"m3_bind", "m3_code", "m3_compile", "m3_core", "m3_env", "m3_exec", "m3_function", "m3_module", "m3_parse", "m3_validate"}
W3_DEFS = ["d_m3HasPosixHost=0", "M3_THREAD_LOCAL=", "M3_HAS_THREAD_LOCAL=0", "d_m3HasAtomics=0",
           "d_m3HasExceptionHandling=0", "d_m3HasSnapshots=0", "d_m3HasStackSwitching=0", "d_m3HasTypedRefs=0",
           "d_m3HasMemory64=0", "d_m3HasMultiMemory=0", "d_m3HasWideArithmetic=0", "d_m3HasExtendedConst=0",
           "d_m3HasCompactImports=0", "d_m3HasRefTypes=0", "d_m3HasGasMetering=0", "d_m3GuardedMemory=0",
           "d_m3VerboseErrorMessages=0", "d_m3MaxFunctionStackHeight=128", "d_m3MaxConstantTableSize=64",
           "d_m3CodePageAlignSize=1024", "d_m3MaxNativeStack=5120", "XIP_WASM3_ALLOC"]


def read_c_registry(path=None):
    """name -> (component number, state bytes, supported chips) from modules/C-MODULES;
    an omitted chips column means every chip."""
    path = path or os.path.join(ROOT, "modules", "C-MODULES")
    reg = {}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.split("#")[0].strip()
            if not line:
                continue
            fields = line.split(":")
            if len(fields) not in (3, 4) or not fields[1].isdigit() or not fields[2].isdigit():
                raise ValueError(f"{path}:{n}: want `name:component_number:state_bytes[:chips]`")
            chips = tuple(fields[3].split(",")) if len(fields) == 4 else tuple(mkmod.CHIPS)
            if not all(c in mkmod.CHIPS for c in chips):
                raise ValueError(f"{path}:{n}: unknown chip in {fields[3]!r}")
            reg[fields[0]] = (int(fields[1]), int(fields[2]), chips)
    return reg


def read_stack(directory_name):
    """The stack a module's deepest handler needs (modules/retained-memory.json)."""
    with open(os.path.join(ROOT, "modules", "retained-memory.json")) as f:
        entry = json.load(f).get(directory_name)
    return int(entry["stack_bytes"]) if isinstance(entry, dict) and "stack_bytes" in entry else 8192


def baked_pointers(tool, elf):
    """Words of .rodata holding an address of the image itself. In a PIC image they are dead pointers
    (no relocation exists to fix them); the count is what a C port to that chip has to design out.
    Some hits are string bytes that happen to look like in-image addresses - the gate is heuristic,
    but a const-table language runtime like Berry produces thousands and no correct PIC image may
    rely on them. Rust modules never emit them (crates/xip-abi/src/task.rs builds pointer tables in
    RAM at run time). objdump -s prints raw bytes: decode each word little-endian."""
    heads = mkmod.run([tool + "objdump", "-h", elf])
    ro, lo, hi = None, None, 0
    for line in heads.splitlines():
        p = re.match(r"\s+\d+ (\.\S+)\s+([0-9a-f]+)\s+([0-9a-f]+)", line)
        if p and p.group(1) in (".text", ".rodata"):
            size, vma = int(p.group(2), 16), int(p.group(3), 16)
            lo = vma if lo is None else min(lo, vma)
            hi = max(hi, vma + size)
            if p.group(1) == ".rodata":
                ro = (vma, size)
    if not ro:
        return 0, []
    hits = []
    dump = mkmod.run([tool + "objdump", "-s", "--section=.rodata", elf])
    for line in dump.splitlines():
        m = re.match(r"\s+([0-9a-f]+)((?: [0-9a-f]{2,8})+)", line)
        if not m:
            continue
        addr = int(m.group(1), 16)
        col = 0
        for w in m.group(2).split():
            if len(w) == 8:
                v = int.from_bytes(bytes.fromhex(w), "little")
                if lo <= v < hi:
                    hits.append((addr + col, v))
            col += len(w) // 2
    return len(hits), hits[:6]


def compile_c(tool, srcs, out, inc, define_cell, extra):
    """Compile: vendored third-party sources as they are (upstream warning waivers, no -Werror),
    the repository's own files under -Wall -Wextra -Werror. The RISC-V caller passes
    --param small-data-limit=0: the module linker scripts claim only .text/.rodata."""
    os.makedirs(out, exist_ok=True)
    objs = []
    for f, c99 in srcs:
        base = os.path.basename(f)[:-2]
        obj = os.path.join(out, base + ".o")
        cmd = [tool + "gcc", "-Os", "-g0", "-ffunction-sections", "-fdata-sections", *extra,
               f"-I{os.path.join(ROOT, 'include')}",
               f"-I{os.path.join(ROOT, 'modules', 'common')}", *[f"-I{i}" for i in inc]]
        if define_cell:
            cmd += [define_cell, "-include", os.path.join(ROOT, "modules", "common", "xip_native_libc.h")]
        cmd += ["-std=c99", "-Wall", "-Wno-strict-aliasing", "-Wno-unused-function"] if c99 else \
               ["-std=c11", "-Wall", "-Wextra", "-Werror"]
        mkmod.run(cmd + ["-c", f, "-o", obj])
        objs.append(obj)
    return objs


def c_link(tool, objs, elf, script, vma, data_vma, fixed):
    cmd = [tool + "gcc", "-nostdlib", "-o", elf, "-Wl,--gc-sections", "-Wl,-e,xip_handle", "-Wl,-u,xip_handle",
           f"-Wl,-T{script}", f"-Wl,--defsym=MODULE_VMA={vma}"]
    if fixed:
        cmd.append(f"-Wl,--defsym=MODULE_DATA_VMA={data_vma}")
    mkmod.run(cmd + objs + ["-lc", "-lgcc"])


def build_c(chip, out, directory, name, tag, state, slot, version=0, autostart=None, keep_elf=None):
    cfg = mkmod.CHIPS[chip]
    static_addr = mkmod.static_address(chip, slot)
    layout = mkmod.read_slot_layout()
    declared, span = layout.get(directory, (slot, 1))
    if slot + span > min(dmod.STATIC_SLOTS, cfg["slots"]):
        sys.exit(f"module span exceeds the virtual window of {chip} ({cfg['slots']} slots)")
    for other, (start, count) in layout.items():
        if other != directory and slot < start + count and start < slot + span:
            sys.exit(f"module slot range overlaps {other}: {slot}..{slot + span - 1}")
    capacity = span * mkmod.WINDOW_SLOT
    stack = read_stack(directory)
    tool = cfg["prefix"] + "-"
    window = (cfg["code_base"] + slot * mkmod.WINDOW_SLOT) if cfg["fixed"] else 0
    data_window = (cfg["data_base"] + slot * mkmod.WINDOW_SLOT) if cfg["fixed"] else 0
    script = os.path.join(ROOT, "modules", "common", "link-fixed.x" if cfg["fixed"] else "link.x")

    mod_dir = os.path.join(ROOT, "modules", directory)
    inc = [os.path.join(mod_dir, "port")]
    srcs = []
    extra_defs = []
    berry = os.path.join(mod_dir, "berry")
    if os.path.isdir(berry):
        inc.append(berry)
        for f in sorted(os.listdir(berry)):
            if f.endswith(".c") and f[:-2] not in SKIP:
                srcs.append((os.path.join(berry, f), True))
    wasm3 = os.path.join(mod_dir, "wasm3")
    if os.path.isdir(wasm3):
        inc.append(wasm3)
        extra_defs = [f"-D{d}" for d in W3_DEFS]
        if "xtensa" in tool:     # its gcc has no sibling-call pattern: `musttail` is an error there, plain calls are correct
            extra_defs.append("-DM3_HAS_TAIL_CALL=0")
        for f in sorted(os.listdir(wasm3)):
            if f.endswith(".c") and f[:-2] in W3_FILES:
                srcs.append((os.path.join(wasm3, f), True))
    srcs.append((os.path.join(mod_dir, f"{directory}.c"), False))
    srcs.append((os.path.join(ROOT, "modules", "common", "xip_native_libc.c"), False))
    port = os.path.join(mod_dir, "port")
    for f in sorted(os.listdir(port)):
        if f.endswith(".c"):
            srcs.append((os.path.join(port, f), False))

    with tempfile.TemporaryDirectory() as tmp:
        extra = extra_defs
        # no small-data param for the cross gccs: riscv parks small constants in .srodata, which
        # its module linker script merges into .rodata; xtensa gcc emits no such sections.
        objs = compile_c(tool, srcs, os.path.join(tmp, "obj"), inc, f"-DXIP_CELL_ADDR={hex(static_addr)}", extra)
        elf = os.path.join(tmp, "module.elf")
        c_link(tool, objs, elf, script, window + dmod.HEADER_SIZE, data_window + dmod.HEADER_SIZE, cfg["fixed"])
        if cfg["fixed"]:
            # .rodata follows .text in flash; its data-bus address is known only after the first pass.
            text = mkmod.section_size(tool, elf, ".text")
            c_link(tool, objs, elf, script, window + dmod.HEADER_SIZE,
                   data_window + dmod.HEADER_SIZE + text, True)
            if mkmod.section_size(tool, elf, ".text") != text:
                sys.exit("the size of .text changed between link passes")

        reloc = mkmod.run([tool + "readelf", "-r", elf])
        if "no relocations" not in reloc.lower():
            sys.exit("module has relocations (it must be position independent or fixed-address):\n" + reloc)
        undefined = "\n".join(l for l in mkmod.run([tool + "nm", "-u", elf]).splitlines() if "no symbols" not in l).strip()
        if undefined:
            sys.exit("module needs symbols it does not define (a module links no libc; use the host table):\n" + undefined)
        for line in mkmod.run([tool + "size", "-A", elf]).splitlines():
            p = line.split()
            if p and re.match(r"\.(s?data|s?bss|tdata|tbss|init_array|fini_array|ctors|dtors|got|dynamic)(\.|$)", p[0]) and int(p[1]):
                sys.exit(f"module has {p[0]} ({p[1]} bytes): the image is read-only flash, no .data/.bss; keep state in the state block")
        if not cfg["fixed"]:
            n, sample = baked_pointers(tool, elf)
            if n:
                sys.exit(f"{n} .rodata words of this PIC image hold their own link-time addresses "
                         f"(C const pointer tables; a moved image cannot use them). Sample: "
                         + ", ".join(f"+0x{a - window:x} -> +0x{v - window:x}" for a, v in sample)
                         + "\nA PIC C module must build such tables in RAM at run time (see crates/xip-abi/src/task.rs).")
        if keep_elf:
            os.makedirs(os.path.dirname(os.path.abspath(keep_elf)), exist_ok=True)
            shutil.copy(elf, keep_elf)
        raw = os.path.join(tmp, "module.bin")
        mkmod.run([tool + "objcopy", "-O", "binary", elf, raw])
        entry = None
        for line in mkmod.run([tool + "readelf", "-h", elf]).splitlines():
            if "Entry point address" in line:
                entry = int(line.split()[-1], 16)
        code = open(raw, "rb").read()
        api_manifest = os.path.join(ROOT, "modules", directory, "API.manifest")
        base_manifest = open(api_manifest, "rb").read() if os.path.exists(api_manifest) else b""
        manifest = mkmod.manifest_with(mkmod.catalog_free(base_manifest) if base_manifest else b"", autostart, name=name, tag=tag, version=version)
        image = dmod.pack(code, entry - window, tag, code_vma=window, data_vma=data_window, stack_bytes=stack,
                          fixed_vma=cfg["fixed"], state_bytes=state, manifest=manifest, static_addr=static_addr)
    if len(image) > capacity:
        sys.exit(f"image is {len(image)} bytes, over its {span}-slot reservation of {capacity}: increase its span in modules/SLOTS")
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, "wb") as f:
        f.write(image)
    print(f"{out}: {len(image)} bytes, module {name} (component {tag}), version {version}, slot {slot}, entry +0x{entry - window:x}")
    return image


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("directory", help="the module directory (modules/mod_x)")
    ap.add_argument("--chip", required=True, choices=sorted(mkmod.CHIPS))
    ap.add_argument("--slot", type=int, help="the module slot (default: modules/SLOTS)")
    ap.add_argument("--version", type=int, help="uint32 version; default: UTC minutes plus the local increasing counter")
    ap.add_argument("--autostart", nargs="?", const="", default=None, metavar="REQUEST",
                    help="start this module at boot with REQUEST as its start request (manifest key 6)")
    ap.add_argument("--elf", help="also keep the linked ELF here")
    ap.add_argument("--out", help="the .dmod to write (default: the module directory's target/out/<chip>/modules)")
    a = ap.parse_args()
    directory = os.path.basename(os.path.abspath(a.directory).rstrip("/"))
    if not directory.startswith("mod_") or not os.path.isdir(os.path.join(ROOT, "modules", directory)):
        ap.error(f"{a.directory} is not a modules/mod_* directory")
    short = directory[4:] if directory.startswith("mod_") else directory
    registry = read_c_registry()
    if short not in registry:
        ap.error(f"{short} is not in modules/C-MODULES (this builder takes C modules only)")
    tag, state, chips = registry[short]
    if a.chip not in chips:
        print(f"{directory}: not built for {a.chip} (modules/C-MODULES says {','.join(chips)}): skipped")
        sys.exit(3)
    slot = a.slot if a.slot is not None else mkmod.read_slots().get(directory)
    if slot is None:
        ap.error(f"{directory} has no slot: add it to modules/SLOTS or pass --slot")
    out = a.out or os.path.join(ROOT, "target", "out", a.chip, "modules", f"{directory}.dmod")
    try:
        a.version = build_version.reserve(ROOT, directory, a.chip, a.version)
    except ValueError as error:
        ap.error(str(error))
    build_c(a.chip, out, directory, short, tag, state, slot, a.version, a.autostart, a.elf)


if __name__ == "__main__":
    main()
