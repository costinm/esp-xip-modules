#!/usr/bin/env python3
"""Build a module image (DMOD) from a Rust bin (--cargo): link, check, pack. (C modules were dropped; the last tree that had them is in git history.)

  mkmod.py --chip esp32c6 --slot 5 --version 3 --out target/modules/esp32c6/mod_rtest.dmod --cargo modules/mod_rtest/Cargo.toml:mod_rtest:flat-image

A module has a starting SLOT and optional span (modules/SLOTS, name start [span], within 0..15).
The start selects its RAM cell and, on Xtensa, its link address; the span reserves consecutive 64 KiB windows.
The slot is independent of flash placement: an image needs enough consecutive free pages for its actual size. docs/abi.md, "Versions and pages".

The toolchain comes from the PATH (an ESP-IDF shell: `. ./env.sh`). Checks that a module must pass
before it is worth signing: no relocations, no undefined symbols, no .data/.bss (the image is read-only
flash), and it fits its page. docs/writing-modules.md explains why.

Wire components: manifest key 13 comes from `--wire ID[:VER],...` (the Cargo.toml mkmod option), which stays the
source of the manifest; `--host-metadata` has no ELF and so cannot check it. A module that exports a static
component table (`export_module!(..., components = [(ID, VER, fn), ...])`) puts it in the `.xip_comp` section;
after linking, that table must equal `--wire` (same IDs and versions, any order), or the build stops. A module
without the section is not checked (it dispatches in its own handle).
"""
import argparse
import json
import os
import shutil
import re
import subprocess
import struct
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dmod  # noqa: E402
import build_version  # noqa: E402

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")

# Where each chip runs modules. Must match components/xip_loader/src/loader.c (tests/test_tools.py checks).
# `slots`: link windows a chip has (the loader's XIP_WINDOW_BYTES); the RAM cells are dmod.STATIC_SLOTS for all.
# esp32c6 and esp32s3 map modules at the same code address; their data addresses differ (the S3 reads data on its own bus).
CHIPS = {
    "esp32":   dict(slots=27, prefix="xtensa-esp32-elf", fixed=True, code_base=0x40250000, data_base=0x3F650000, rust_target="xtensa-esp32-espidf"),
    "esp32s3": dict(slots=64, prefix="xtensa-esp32s3-elf", fixed=True, code_base=0x42400000, data_base=0x3C400000, rust_target="xtensa-esp32s3-espidf"),
    # C6 has one linear flash region that both buses read (SOC_IRAM0/DRAM0 share the vaddrs), so the
    # data window is the code window; it starts above the pages Stage2 maps for Main (<= 0x42280000).
    "esp32c6": dict(slots=64, prefix="riscv32-esp-elf", fixed=True, code_base=0x42400000, data_base=0x42400000,
                    rust_target="riscv32imac-unknown-none-elf"),
}


WINDOW_SLOT = 0x10000   # one MMU page: the link-address window of a module (xip_check.h, XIP_DMOD_WINDOW_SLOT_BYTES)


def static_address(chip, slot):
    """The module's slot chooses its declared RAM cell (and on Xtensa its link window); the flash page it is installed on does not."""
    if not isinstance(slot, int) or slot < 0:
        raise ValueError("module slot must be a nonnegative integer")
    if slot >= dmod.STATIC_SLOTS:
        raise ValueError(f"module RAM arena has only {dmod.STATIC_SLOTS} cells")
    return dmod.STATIC_BASES[chip] + slot * dmod.STATIC_SLOT_BYTES


def read_slot_layout(path=None):
    """name start [span] registry. Span reserves consecutive 64 KiB windows;
    only the first slot supplies the allocator/state RAM cell."""
    path = path or os.path.join(ROOT, "modules", "SLOTS")
    layout = {}
    occupied = {}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            fields = line.split("#")[0].split()
            if not fields:
                continue
            if len(fields) not in (2, 3) or not all(x.isdigit() for x in fields[1:]):
                raise ValueError(f"{path}:{n}: want `name start [span]`")
            name, start = fields[0], int(fields[1])
            span = int(fields[2]) if len(fields) == 3 else 1
            if name in layout or span < 1 or start + span > dmod.STATIC_SLOTS:
                raise ValueError(f"{path}:{n}: invalid or duplicate slot range for {name}")
            for slot in range(start, start + span):
                if slot in occupied:
                    raise ValueError(f"{path}:{n}: {name} overlaps {occupied[slot]} at slot {slot}")
                occupied[slot] = name
            layout[name] = (start, span)
    return layout


def read_slots(path=None):
    return {name: start for name, (start, span) in read_slot_layout(path).items()}


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


def _cbor_item(data, pos):
    """Just enough CBOR to read a manifest's name and component number back: (value, next position)."""
    major, info = data[pos] >> 5, data[pos] & 31
    pos += 1
    n = info
    if info == 24:
        n, pos = data[pos], pos + 1
    elif info == 25:
        n, pos = int.from_bytes(data[pos:pos + 2], "big"), pos + 2
    elif info == 26:
        n, pos = int.from_bytes(data[pos:pos + 4], "big"), pos + 4
    if major == 0:
        return n, pos
    if major == 3:
        return data[pos:pos + n].decode(), pos + n
    if major == 4:
        out = []
        for _ in range(n):
            v, pos = _cbor_item(data, pos)
            out.append(v)
        return out, pos
    if major == 5:
        out = {}
        for _ in range(n):
            k, pos = _cbor_item(data, pos)
            out[k], pos = _cbor_item(data, pos)
        return out, pos
    if major == 7:
        return n == 21, pos
    raise ValueError(major)


def manifest_identity(base):
    """(name, component number) of a generated manifest: keys 1 and 2."""
    m, _ = _cbor_item(base, 0)
    return m.get(1), m.get(2)


def with_version(base, version):
    """`base` (a definite-length CBOR map starting {1: name, ...}) with `8: version` right behind the name, so a reader that needs only the
    name and the version (mod_flash reads 48 bytes) finds both at the front. An existing key 8 is replaced."""
    count = base[0] - 0xa0
    if base[0] & 0xe0 != 0xa0 or count > 23 or base[1:2] != b"\x01":
        raise ValueError("a manifest is a small definite-length map starting with the name")
    _, name_end = _cbor_item(base, 2)           # past key 1's value
    rest, pos, kept = b"", name_end, 0
    for _ in range(count - 1):
        start = pos
        key, pos = _cbor_item(base, pos)
        _, pos = _cbor_item(base, pos)
        if key != 8:
            rest += base[start:pos]
            kept += 1
    return bytes([0xa0 + kept + 2]) + base[1:name_end] + cbor(8) + cbor(version) + rest


def _wire_entry_ok(entry):
    if len(entry) != 2:
        return False
    c, v = entry
    return all(isinstance(x, int) and not isinstance(x, bool) for x in (c, v)) and 0 < c <= 0x7fffffff and 0 <= v <= 0xffffffff


def parse_wire(text):
    """`--wire ID:VERSION[,ID:VERSION]` as [(id, version)]. The version defaults to 1 when ':VERSION' is omitted."""
    entries = []
    for item in filter(None, text.split(",")):
        ident, sep, version = item.partition(":")
        try:
            entries.append((int(ident), int(version) if sep else 1))
        except ValueError:
            sys.exit(f"--wire: {item!r} is not ID or ID:VERSION")
    return entries


def catalog_free(base, name=None):
    """`base`, the generated map of a module's API.md (`mesh-api-gen --out-manifest`), with only keys 1 (name) and 2 (component number).
    `name` (`--name`) replaces the name of key 1. Keys 3..5 (methods, field lists, records) are the API catalog for tools: the schemas under
    api/generated are the source the clients read, and no firmware reads the catalog from an image, so an image does not carry it."""
    count = base[0] - 0xa0
    if base[0] & 0xe0 != 0xa0 or count > 23:
        raise ValueError("a manifest is a small definite-length map")
    kept, rest, pos = [], b"", 1
    for _ in range(count):
        start = pos
        key, pos = _cbor_item(base, pos)
        _, pos = _cbor_item(base, pos)
        if key in (1, 2):
            kept.append(key)
            rest += (cbor(1) + cbor(name)) if key == 1 and name is not None else base[start:pos]
    if kept != [1, 2]:
        raise ValueError("a generated manifest starts with its name (key 1) and component number (key 2)")
    return bytes([0xa2]) + rest


def manifest_with(base, autostart=None, name=None, tag=None, port=0, stream=0, version=0, components=(), wire=()):
    """The manifest of a module (include/xip.h, module_manifest): its name and component number (keys 1 and 2 of `base`, see
    `catalog_free`), plus what only this build knows:

      6: start request     (the module starts at boot: text, may be empty)
      9: port              (an integer: the datagram port whose packets the module receives and answers; the runtime binds it at start. 1: the console)
      10: stream port      (an integer: TCP port PORT_NET + n; connections arrive as socket events the module reads and writes)
      11: components       (an array of texts: the components the module hosts, at most 3 of at most 15 bytes. The runtime asks the module for the
                            table of each after it started and serves it to `get_component(name)` callers through thunks.)
      13: components       (an array of up to 8 [id, version] pairs, the wire components this module serves: id a distinct positive
                            component ID (at most 0x7fffffff), version a uint32, 0 or more. Encoded as CBOR arrays of 2-element arrays.)
      8: version           (an integer: higher is newer, no other meaning; absent is 0 and not written. The loader runs the highest
                            version of a name. Written right behind the name.)

    `base` is a map that starts with the name and the component number (`catalog_free` of the generated one). A module without an API.md has none:
    its manifest is then `{1: name, 2: component number}` from `name` and `tag`, because the loader finds a module by scanning and reads its name here.
    """
    if not base:
        if name is None or tag is None:
            sys.exit("a module without an API.md needs --name and --tag")
        base = cbor({1: name, 2: tag})
    count = base[0] - 0xa0
    if base[0] & 0xe0 != 0xa0 or count > 21:
        raise ValueError("a manifest is a small definite-length map")
    extra = b""
    if autostart is not None:
        extra += cbor(6) + cbor(autostart)
        count += 1
    if port:
        extra += cbor(9) + cbor(port)
        count += 1
    if stream:
        extra += cbor(10) + cbor(stream)
        count += 1
    if components:
        if len(components) > 3 or any(not c or len(c.encode()) > 15 for c in components):
            sys.exit("--components: at most 3 names of 1 to 15 bytes")
        extra += cbor(11) + cbor(list(components))
        count += 1
    if wire:
        if len(wire) > 8 or len({e[0] for e in wire if len(e) == 2}) != len(wire) or not all(_wire_entry_ok(e) for e in wire):
            sys.exit("--wire: at most 8 entries of distinct component IDs (1 to 0x7fffffff), each with a version (uint32)")
        extra += cbor(13) + cbor([[c, v] for c, v in wire])
        count += 1
    manifest = bytes([0xa0 + count]) + base[1:] + extra
    return with_version(manifest, version) if version else manifest


def manifest_beside(api_path, autostart=None, port=0, stream=0, version=0, components=(), wire=(), name=None):
    """The manifest of the module whose API.md is `api_path`: the checked-in file beside it, `API.manifest`, without its catalog."""
    path = os.path.splitext(api_path)[0] + ".manifest"
    if not os.path.exists(path):
        sys.exit(f"{path} is missing: run scripts/gen.sh (it needs mesh-api-gen from ssh-mesh)")
    with open(path, "rb") as f:
        return manifest_with(catalog_free(f.read(), name), autostart, port=port, stream=stream, version=version, components=components, wire=wire)


def api_beside(sources, cargo):
    """The API.md next to the module's Cargo.toml."""
    first = cargo[0] if cargo else None
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


def parse_component_table(data):
    """The `.xip_comp` bytes of a module: LE (i32 component ID, u32 version) pairs, at most 8, as export_module! writes them."""
    if len(data) % 8 or len(data) // 8 > 8:
        sys.exit(f".xip_comp is {len(data)} bytes: want at most 8 (i32 ID, u32 version) pairs")
    entries = [struct.unpack_from("<iI", data, pos) for pos in range(0, len(data), 8)]
    if any(c <= 0 for c, _ in entries) or len({c for c, _ in entries}) != len(entries):
        sys.exit(f".xip_comp {entries}: component IDs must be distinct and positive")
    return entries


def component_table(tool, elf, tmp):
    """[(id, version)] of the ELF's `.xip_comp` section, or None when the module has no such section (no table: nothing to check)."""
    if not re.search(r"\]\s+\.xip_comp\s", run([tool + "readelf", "-SW", elf])):
        return None
    raw = os.path.join(tmp, "xip_comp.bin")
    run([tool + "objcopy", "-O", "binary", "-j", ".xip_comp", elf, raw])
    with open(raw, "rb") as f:
        return parse_component_table(f.read())


def cargo_link(cfg, cargo, window, link_script, tmp, static_addr):
    """Return link(data_vma) -> elf for a Rust module: `cargo rustc` of the bin with the module linker script.

    cargo = (manifest, bin, features). Needs the Rust toolchain with the ESP targets on the PATH (rust-src for
    -Zbuild-std). Fixed-window chips link like C (relocation-model=static); only a PIC chip uses
    relocation-model=pic.
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
        # -Zlocation-detail=none: a panic location is a static with a pointer to the file name, which a module image (no relocations) cannot hold
        rustflags = ["-C", "opt-level=s", "-C", "lto=fat", "-C", "codegen-units=1", "-Zlocation-detail=none", *extra,
                     "-C", f"link-arg=-Wl,--defsym=MODULE_VMA={window + dmod.HEADER_SIZE}",
                     "-C", "link-arg=-Wl,-e,xip_handle", "-C", "link-arg=-Wl,-u,xip_handle",
                     "-C", f"link-arg=-T{link_script}", "-C", "link-arg=-nostdlib", "-C", "link-arg=-Wl,--gc-sections"]
        if cfg["fixed"]:
            rustflags += ["-C", f"link-arg=-Wl,--defsym=MODULE_DATA_VMA={data_vma}"]
        # cargo fingerprints linker arguments poorly: make the second pass really relink.
        os.utime(os.path.join(srcdir, "bin", binname + ".rs"))
        env = dict(os.environ)
        env["XIP_MODULE_STATIC_ADDR"] = hex(static_addr)
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


MAX_IMAGE = WINDOW_SLOT   # default capacity; modules/SLOTS may reserve a larger consecutive span.


def module_manifest(cargo, name=None, tag=None, autostart=None, port=0, stream=0, version=0, components=(), wire=()):
    """The canonical image/native-artifact manifest and identity."""
    api = api_beside(None, cargo)
    if api:
        api_name, api_tag = manifest_identity(open(os.path.splitext(api)[0] + ".manifest", "rb").read())
        name, tag = name or api_name, tag or api_tag
        if tag != api_tag:
            sys.exit(f"--tag {tag} is not the component number {api_tag} of its API.md")
        manifest = manifest_beside(api, autostart, port, stream, version, components, wire, name)
    else:
        manifest = manifest_with(b"", autostart, name, tag, port, stream, version, components, wire)
    if not tag or not name:
        sys.exit("a module needs a name and a component number (its API.md, or --name and --tag)")
    return manifest, name, tag


def build(chip, out, name=None, tag=None, slot=0, stack=8192, state=0, keep_elf=None, cargo=None,
          autostart=None, port=0, stream=0, version=0, components=(), wire=()):
    """`slot` selects the module's RAM cell on every chip. Every chip also links code for code_base + slot * 64 KiB; the image can then be
    installed on any flash page, the loader maps that page at the module's window slot. (PIC linking stays
    available for a chip without fixed windows; nothing ships PIC today.)"""
    cfg = CHIPS[chip]
    static_addr = static_address(chip, slot)
    module_key = os.path.basename(os.path.dirname(cargo[0])) if cargo else ""
    layout = read_slot_layout()
    declared, span = layout.get(module_key, (slot, 1))
    if slot + span > min(dmod.STATIC_SLOTS, cfg["slots"]):
        sys.exit(f"module span exceeds the virtual window of {chip} ({cfg['slots']} slots)")
    for other, (start, count) in layout.items():
        if other != module_key and slot < start + count and start < slot + span:
            sys.exit(f"module slot range overlaps {other}: {slot}..{slot + span - 1}")
    capacity = span * WINDOW_SLOT
    tool = cfg["prefix"] + "-"
    window = (cfg["code_base"] + slot * WINDOW_SLOT) if cfg["fixed"] else 0          # image start
    data_window = (cfg["data_base"] + slot * WINDOW_SLOT) if cfg["fixed"] else 0
    link_script = os.path.join(ROOT, "modules", "common", "link-fixed.x" if cfg["fixed"] else "link.x")
    with tempfile.TemporaryDirectory() as tmp:
        elf = os.path.join(tmp, "module.elf")
        link = cargo_link(cfg, cargo, window, link_script, tmp, static_addr)

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
        table = component_table(tool, elf, tmp)
        if table is not None and sorted(table) != sorted(wire):
            sys.exit(f"--wire {sorted(wire)} does not match the module's .xip_comp table {sorted(table)}: "
                     "key 13 comes from --wire, the table from export_module!(components = ...); make them the same")
        if keep_elf:
            shutil.copy(elf, keep_elf)
        raw = os.path.join(tmp, "module.bin")
        run([tool + "objcopy", "-O", "binary", elf, raw])
        entry = None
        for line in run([tool + "readelf", "-h", elf]).splitlines():
            if "Entry point address" in line:
                entry = int(line.split()[-1], 16)
        code = open(raw, "rb").read()
        manifest, name, tag = module_manifest(cargo, name, tag, autostart, port, stream, version, components, wire)
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
    ap.add_argument("--chip", choices=sorted(CHIPS))
    ap.add_argument("--out")
    ap.add_argument("--host-metadata", metavar="JSON", help="write native state/manifest metadata without compiling an image")
    ap.add_argument("--name", help="the module's name (default: from its API.md)")
    ap.add_argument("--tag", type=int, help="the module's component number (default: from its API.md)")
    ap.add_argument("--slot", type=int, default=None,
                    help="the module's slot (modules/SLOTS by default, via its name): its RAM cell and, on Xtensa, its 64 KiB link window; not a flash page")
    ap.add_argument("--version", type=int,
                    help="uint32 manifest version; default: UTC minutes since 2026-01-01, increased by the local target counter; 0: unversioned")
    ap.add_argument("--stack", type=int, default=8192, help="stack the deepest handler needs, bytes")
    ap.add_argument("--state", type=int, default=0, help="size of the zeroed state block passed to every call, bytes (<= 4096)")
    ap.add_argument("--autostart", nargs="?", const="", default=None, metavar="REQUEST",
                    help="start this module at boot (recorded in its manifest) with REQUEST as its start request")
    ap.add_argument("--port", type=int, default=0, help="the port the module receives frames from and answers on (manifest key 9; 1: the console)")
    ap.add_argument("--stream", type=int, default=0, help="the stream port: TCP port PORT_NET + N, connections arrive as socket events (manifest key 10)")
    ap.add_argument("--components", default="", metavar="NAME[,NAME]",
                    help="the components the module hosts (manifest key 11): the runtime asks for their tables and serves them to get_component")
    ap.add_argument("--wire", default="", metavar="ID:VERSION[,ID:VERSION]",
                    help="the wire components this module serves, each with its version (manifest key 13; the version defaults to 1). "
                         "The manifest source: an image builds only if its .xip_comp table, when it has one, is the same set; "
                         "--host-metadata cannot check the table (no ELF)")
    ap.add_argument("--elf", help="also keep the linked ELF here (objdump -d)")
    ap.add_argument("--cargo", required=True, metavar="MANIFEST:BIN[:FEATURES]",
                    help="the Rust module to build: the bin BIN of the crate at MANIFEST (e.g. modules/mod_lora/Cargo.toml:mod_lora:flat-image)")
    a = ap.parse_args()
    parts = a.cargo.split(":")
    if len(parts) < 2:
        ap.error("--cargo wants MANIFEST:BIN[:FEATURES]")
    cargo = (os.path.abspath(parts[0]), parts[1], parts[2] if len(parts) > 2 else "")
    if a.version is not None and not 0 <= a.version <= 0xffffffff:
        ap.error("--version is a 32-bit unsigned number")
    if a.host_metadata:
        a.version = a.version if a.version is not None else 0
        manifest, _, _ = module_manifest(cargo, a.name, a.tag, a.autostart, a.port, a.stream, a.version, [c for c in a.components.split(',') if c], parse_wire(a.wire))
        os.makedirs(os.path.dirname(os.path.abspath(a.host_metadata)), exist_ok=True)
        with open(a.host_metadata, "w") as f:
            json.dump({"state_bytes": a.state, "manifest": list(manifest)}, f)
            f.write("\n")
        return
    if not a.chip or not a.out:
        ap.error("image builds require --chip and --out")
    slot = a.slot
    if slot is None:
        slot = read_slots().get(os.path.basename(os.path.dirname(cargo[0])))
        if slot is None:
            ap.error("no slot: pass --slot or list the module in modules/SLOTS")
    # Seed new counters from a previous local artifact, including pinned release versions.
    previous_version = 0
    try:
        with open(a.out, 'rb') as old:
            old_manifest, _ = _cbor_item(dmod.manifest_of(old.read()), 0)
            previous_version = old_manifest.get(8, 0)
    except (OSError, ValueError, IndexError, TypeError, AttributeError, struct.error):
        pass  # No valid previous artifact at this output path.
    try:
        a.version = build_version.reserve(ROOT, cargo[1], a.chip, a.version, minimum=previous_version)
    except ValueError as error:
        ap.error(str(error))
    build(a.chip, a.out, a.name, a.tag, slot, a.stack, a.state, keep_elf=a.elf,
          cargo=cargo, autostart=a.autostart, port=a.port, stream=a.stream, version=a.version,
          components=[c for c in a.components.split(',') if c], wire=parse_wire(a.wire))


if __name__ == "__main__":
    main()
