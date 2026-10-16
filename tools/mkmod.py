#!/usr/bin/env python3
"""Build a module image (DMOD) from C sources (or a Rust bin with --cargo): compile, link, check, pack.

  mkmod.py --chip esp32c6 --slot test --out target/modules/esp32c6/mod_test.dmod modules/mod_test/mod_test.c
  mkmod.py --chip esp32 --list-slots

The toolchain comes from the PATH (an ESP-IDF shell: `. ./env.sh`). Checks that a module must pass
before it is worth signing: no relocations, no undefined symbols, no .data/.bss (the image is read-only
flash), and it fits its slot. docs/writing-modules.md explains why.
"""
import argparse
import os
import shutil
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dmod  # noqa: E402

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")

# Where each chip runs modules. Must match components/xip_loader/src/loader.c (tests/test_tools.py checks).
CHIPS = {
    "esp32":   dict(prefix="xtensa-esp32-elf", fixed=True, code_base=0x40300000, data_base=0x3F700000,
                    arch=["-mtext-section-literals"], rust_target="xtensa-esp32-espidf"),
    "esp32s3": dict(prefix="xtensa-esp32s3-elf", fixed=True, code_base=0x43000000, data_base=0x3D000000,
                    arch=["-mtext-section-literals"], rust_target="xtensa-esp32s3-espidf"),
    "esp32c6": dict(prefix="riscv32-esp-elf", fixed=False, code_base=0, data_base=0,
                    arch=["-march=rv32imac_zicsr_zifencei", "-mabi=ilp32", "-fPIE", "-mno-relax"],
                    rust_target="riscv32imac-unknown-none-elf"),
}
CFLAGS = ["-std=gnu11", "-Os", "-Wall", "-Wextra", "-Werror", "-ffreestanding", "-fno-builtin", "-nostdlib",
          "-fno-stack-protector", "-fno-asynchronous-unwind-tables", "-fno-unwind-tables", "-ffunction-sections",
          "-fdata-sections", "-fno-common", "-fno-jump-tables", "-fvisibility=hidden",
          "-fno-tree-loop-distribute-patterns"]


def slots(path=os.path.join(ROOT, "crates", "xip-runtime-esp", "src", "loader.rs")):
    """The product slot table shared by the C and Rust examples: {name: (tag, offset, size)}. A C main lists {"name", tag, offset, size} in main_slots[];
    a Rust one lists slot!("name", tag, offset, size) (xip-runtime-esp), anywhere in the file."""
    with open(path) as source:
        text = source.read()
    # Compatibility with callers selecting the C example explicitly. Its C
    # initializer is now generated from the shared product table.
    if (os.path.realpath(path) == os.path.realpath(os.path.join(ROOT, "main", "main.c"))
            and '#include "product-slots.inc"' in text):
        return slots()
    if "main_slots[]" in text:
        block = text[text.index("main_slots[]"):]
        block = block[:block.index("};")]
        pattern = r'\{\s*"(\w+)"\s*,\s*(\w+)\s*,\s*(\w+)\s*,\s*(\w+)\s*\}'
    else:
        block = text
        pattern = r'slot!\(\s*"(\w+)"\s*,\s*(\w+)\s*,\s*(\w+)\s*,\s*(\w+)\s*\)'
    return {m[0]: (int(m[1], 0), int(m[2], 0), int(m[3], 0)) for m in re.findall(pattern, block)}


def _cbor_head(major, n):
    if n < 24:
        return bytes([major << 5 | n])
    if n < 256:
        return bytes([major << 5 | 24, n])
    if n < 65536:
        return bytes([major << 5 | 25]) + n.to_bytes(2, "big")
    return bytes([major << 5 | 26]) + n.to_bytes(4, "big")


def cbor(obj):
    """Definite-length CBOR of ints, strings, lists and dicts (the manifest needs nothing else)."""
    if isinstance(obj, bool):
        return bytes([0xf5 if obj else 0xf4])
    if isinstance(obj, int):
        return _cbor_head(0, obj) if obj >= 0 else _cbor_head(1, -1 - obj)
    if isinstance(obj, str):
        return _cbor_head(3, len(obj.encode())) + obj.encode()
    if isinstance(obj, (list, tuple)):
        return _cbor_head(4, len(obj)) + b"".join(cbor(x) for x in obj)
    if isinstance(obj, dict):
        return _cbor_head(5, len(obj)) + b"".join(cbor(k) + cbor(v) for k, v in obj.items())
    raise TypeError(obj)


def manifest_of_api(api_text, autostart=None, flash_slots=()):
    """The manifest of a module (abi/API.md, module_manifest): the catalog of its API.md in compact CBOR, what the console
    needs instead of IDs compiled into it. b"" when the file describes no module.

      {1: name, 2: component,
       3: [{1: method id, 2: method name, 3: request fields (index), 4: response fields (index)}, ...],
       4: [[type name, [fields]], ...],                      (record types the fields refer to; absent if none)
       5: [[fields], ...],                                   (the field lists the methods point at, each stored once)
       6: start request,                                     (only if the module starts at boot: text, may be empty)
       7: flash slot indices}                                (only if flash writes are declared)
      field = [tag, name, kind]  or  [tag, name, kind, type name]
      kind  = 0 any, 1 text, 2 bool, 3 uint, 4 int, 5 bytes, 6 map, 7 array   (the kinds of the console's text syntax)
    """
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from apimd import module_api, module_types
    from gen_catalog import kind_of
    comps = module_api(api_text)
    if not comps:
        return b""
    types = module_types(api_text)
    records = {name: i for i, name in enumerate(sorted(types))}

    def fields(rows):
        out = []
        for tag, fname, ftype in rows:
            kind, rec = kind_of(ftype, records)
            out.append([tag, fname, kind] + ([sorted(types)[rec - 1]] if rec else []))
        return out

    name, cid, methods = comps[0]
    lists = []

    def index(rows):                       # shared request/response tables are stored once
        f = fields(rows)
        if f not in lists:
            lists.append(f)
        return lists.index(f)

    doc = {1: name, 2: cid, 3: [{1: mid, 2: mname, 3: index(req), 4: index(resp)} for mname, mid, _s, req, resp in methods]}
    if types:
        doc[4] = [[tname, fields(types[tname])] for tname in sorted(types)]
    doc[5] = lists
    if autostart is not None:
        doc[6] = autostart
    if flash_slots:
        doc[7] = sorted(set(flash_slots))
    return cbor(doc)


def api_beside(sources, cargo):
    """The API.md next to the module's sources (or its Cargo.toml)."""
    first = cargo[0] if cargo else (sources[0] if sources else None)
    path = os.path.join(os.path.dirname(os.path.abspath(first)), "API.md") if first else None
    return path if path and os.path.exists(path) else None


def run(cmd, **kw):
    r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, **kw)
    if r.returncode:
        sys.exit(f"{' '.join(cmd[:2])} ... failed:\n{r.stdout}")
    return r.stdout


def section_size(tool, elf, name):
    for line in run([tool + "size", "-A", elf]).splitlines():
        parts = line.split()
        if parts and parts[0] == name:
            return int(parts[1])
    return 0


def cargo_link(cfg, cargo, window, link_script, tmp):
    """Return link(data_vma) -> elf for a Rust module: `cargo rustc` of the bin with the module linker script.

    cargo = (manifest, bin, features). Needs the Rust toolchain with the ESP targets on the PATH (rust-src for
    -Zbuild-std). Xtensa Rust cannot produce PIC: it is linked at the slot's fixed address (like C).
    """
    manifest, binname, features = cargo
    features = ",".join(filter(None, [features, "xip-abi/global-alloc", "xip-abi/allocator-api"]))
    target = cfg["rust_target"]
    tool = cfg["prefix"] + "-"
    srcdir = os.path.join(os.path.dirname(os.path.abspath(manifest)), "src")
    elf = os.path.join(os.path.abspath(tmp), "module.elf")

    def link(data_vma):
        extra = ["-C", "relocation-model=static", "-C", "llvm-args=--jump-table-density=1000000",
                 "-C", "llvm-args=--min-jump-table-entries=1000000"] if cfg["fixed"] else ["-C", "relocation-model=pic"]
        rustflags = ["-C", "opt-level=s", "-C", "lto=fat", "-C", "codegen-units=1", *extra,
                     "-C", f"link-arg=-Wl,--defsym=MODULE_VMA={window + dmod.HEADER_SIZE}",
                     "-C", "link-arg=-Wl,-e,xip_handle", "-C", "link-arg=-Wl,-u,xip_handle",
                     "-C", f"link-arg=-T{link_script}", "-C", "link-arg=-nostdlib", "-C", "link-arg=-Wl,--gc-sections"]
        if cfg["fixed"]:
            rustflags += ["-C", f"link-arg=-Wl,--defsym=MODULE_DATA_VMA={data_vma}"]
        # cargo fingerprints linker arguments poorly: make the second pass really relink.
        os.utime(os.path.join(srcdir, "bin", binname + ".rs"))
        env = dict(os.environ)
        if env.get("XIP_RUST_ESP_BIN"):   # env.sh: the esp toolchain (nightly with rust-src) shared with the ESP-IDF install
            env["PATH"] = env["XIP_RUST_ESP_BIN"] + os.pathsep + env["PATH"]
        run(["cargo", "rustc", "--manifest-path", manifest, "--bin", binname, "--features", features,
             "--target", target, "--release", "-Zbuild-std=core,alloc,compiler_builtins",
             "-Zbuild-std-features=compiler-builtins-mem", "--config", f'target.{target}.linker="{tool}gcc"',
             "--", *rustflags], env=env)
        built = os.path.join(os.path.dirname(os.path.abspath(manifest)), "target", target, "release", binname)
        target_dir = os.environ.get("CARGO_TARGET_DIR")
        if target_dir:
            built = os.path.join(target_dir, target, "release", binname)
        shutil.copy(built, elf)
        return elf

    return link


def build(chip, slot, sources, out, stack=8192, features=0, state=0, defines=(), keep_elf=None, cargo=None, slot_file=None,
          autostart=None, flash_slots=()):
    cfg = CHIPS[chip]
    layout = slots(slot_file) if slot_file else slots()
    tag, offset, slot_size = layout[slot]
    if flash_slots:
        if not features & dmod.FEATURES["FLASH"]:
            sys.exit("--flash-slots requires --features FLASH")
        if any(index < 0 or index >= len(layout) or list(layout)[index] == slot for index in flash_slots):
            sys.exit("--flash-slots must name existing slots other than the module's own slot")
    tool = cfg["prefix"] + "-"
    window = (cfg["code_base"] + offset) if cfg["fixed"] else 0          # image start
    data_window = (cfg["data_base"] + offset) if cfg["fixed"] else 0
    link_script = os.path.join(ROOT, "modules", "common", "link-fixed.x" if cfg["fixed"] else "link.x")
    with tempfile.TemporaryDirectory() as tmp:
        objs = []
        for src in ([] if cargo else list(sources) + [os.path.join(ROOT, "modules", "common", "xip_rt.c")]):
            obj = os.path.join(tmp, os.path.basename(src) + ".o")
            run([tool + "gcc", *CFLAGS, *cfg["arch"], *[f"-D{d}" for d in defines],
                 "-I", os.path.join(ROOT, "include"), "-I", os.path.join(ROOT, "modules", "common"),
                 "-c", src, "-o", obj])
            objs.append(obj)
        elf = os.path.join(tmp, "module.elf")

        def link_c(data_vma):
            args = [tool + "gcc", *cfg["arch"], "-nostdlib", "-static", "-Wl,--gc-sections", "-Wl,--no-relax",
                    "-Wl,-z,norelro", "-Wl,--build-id=none", "-Wl,-e,xip_handle", "-Wl,-u,xip_handle",
                    f"-Wl,-T,{link_script}", f"-Wl,--defsym=MODULE_VMA={window + dmod.HEADER_SIZE}"]
            if cfg["fixed"]:
                args.append(f"-Wl,--defsym=MODULE_DATA_VMA={data_vma}")
            run(args + objs + ["-o", elf])
            return elf

        link = cargo_link(cfg, cargo, window, link_script, tmp) if cargo else link_c

        link(data_window + dmod.HEADER_SIZE)
        if cfg["fixed"]:
            # The read-only data follows .text in flash, so its data-bus address is known only now.
            text = section_size(tool, elf, ".text")
            link(data_window + dmod.HEADER_SIZE + text)
            if section_size(tool, elf, ".text") != text:
                sys.exit("the size of .text changed between link passes")

        reloc = run([tool + "readelf", "-r", elf])
        if "no relocations" not in reloc.lower():
            sys.exit("module has relocations (it must be position independent or fixed-address):\n" + reloc)
        undefined = "\n".join(l for l in run([tool + "nm", "-u", elf]).splitlines() if "no symbols" not in l).strip()  # a stripped ELF has none
        if undefined:
            sys.exit("module needs symbols it does not define (a module links no libc; use the host table):\n" + undefined)
        for line in run([tool + "size", "-A", elf]).splitlines():
            p = line.split()
            if p and re.match(r"\.(s?data|s?bss|tdata|tbss|init_array|fini_array|ctors|dtors|got|dynamic)(\.|$)", p[0]) and int(p[1]):
                sys.exit(f"module has {p[0]} ({p[1]} bytes): the image is read-only flash, no .data/.bss; keep state in the state block (--state)")
        if keep_elf:
            shutil.copy(elf, keep_elf)
        raw = os.path.join(tmp, "module.bin")
        run([tool + "objcopy", "-O", "binary", elf, raw])
        entry = None
        for line in run([tool + "readelf", "-h", elf]).splitlines():
            if "Entry point address" in line:
                entry = int(line.split()[-1], 16)
        code = open(raw, "rb").read()
        api = api_beside(sources, cargo)
        manifest = manifest_of_api(open(api).read(), autostart, flash_slots) if api else b""
        if (autostart is not None or flash_slots) and not api:
            sys.exit("--autostart and --flash-slots require an API.md next to the sources")
        image = dmod.pack(code, entry - window, tag, code_vma=window, data_vma=data_window, stack_bytes=stack,
                          required_features=features, fixed_vma=cfg["fixed"], state_bytes=state, manifest=manifest)
    if len(image) > slot_size:
        sys.exit(f"image is {len(image)} bytes, slot '{slot}' holds {slot_size}")
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, "wb") as f:
        f.write(image)
    print(f"{out}: {len(image)} bytes, slot {slot} (tag {tag}), entry +0x{entry - window:x}")
    return image


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--chip", required=True, choices=sorted(CHIPS))
    ap.add_argument("--slot", help="module slot of main (see --list-slots)")
    ap.add_argument("--out")
    ap.add_argument("--stack", type=int, default=8192, help="stack the deepest handler needs, bytes")
    ap.add_argument("--state", type=int, default=0, help="size of the zeroed state block passed to every call, bytes (<= 4096)")
    ap.add_argument("--features", default="0", help="XIP_FEAT_* bits the module needs, e.g. SPI,SETTINGS")
    ap.add_argument("--slots", metavar="FILE",
                    help="C or Rust product slot table (default: crates/xip-runtime-esp/src/loader.rs)")
    ap.add_argument("--list-slots", action="store_true")
    ap.add_argument("--autostart", nargs="?", const="", default=None, metavar="REQUEST",
                    help="start this module at boot (recorded in its manifest) with REQUEST as its start request")
    ap.add_argument("--flash-slots", default="", metavar="INDICES",
                    help="comma-separated module slot indices this image may erase/write (signed manifest key 7)")
    ap.add_argument("--elf", help="also keep the linked ELF here (objdump -d)")
    ap.add_argument("--cargo", metavar="MANIFEST:BIN[:FEATURES]",
                    help="build a Rust module: the bin BIN of the crate at MANIFEST (e.g. fw/mod_lora/Cargo.toml:mod_lora:flat-image)")
    ap.add_argument("sources", nargs="*")
    a = ap.parse_args()
    if a.list_slots:
        for name, (tag, off, size) in (slots(a.slots) if a.slots else slots()).items():
            print(f"{name}: tag {tag} offset 0x{off:x} size 0x{size:x}")
        return
    cargo = None
    if a.cargo:
        parts = a.cargo.split(":")
        if len(parts) < 2:
            ap.error("--cargo wants MANIFEST:BIN[:FEATURES]")
        cargo = (os.path.abspath(parts[0]), parts[1], parts[2] if len(parts) > 2 else "")
    if not (a.slot and a.out and (a.sources or cargo)):
        ap.error("--slot, --out and at least one source (or --cargo) are required")
    feat = 0
    for item in a.features.split(","):
        feat |= dmod.FEATURES[item.upper()] if item.upper() in dmod.FEATURES else int(item, 0)
    try:
        flash_slots = tuple(int(item, 0) for item in a.flash_slots.split(",") if item)
    except ValueError:
        ap.error("--flash-slots requires comma-separated integer indices")
    build(a.chip, a.slot, a.sources, a.out, a.stack, feat, a.state, keep_elf=a.elf, cargo=cargo, slot_file=a.slots,
          autostart=a.autostart, flash_slots=flash_slots)


if __name__ == "__main__":
    main()
