#!/usr/bin/env python3
"""Build a module image (DMOD) from a Rust bin (--cargo): link, check, pack. (C modules were dropped; the last tree that had them is in git history.)

  mkmod.py --chip esp32c6 --out target/modules/esp32c6/mod_rtest.dmod --cargo modules/mod_rtest/Cargo.toml:mod_rtest:flat-image

The toolchain comes from the PATH (an ESP-IDF shell: `. ./env.sh`). Checks that a module must pass
before it is worth signing: no relocations, no undefined symbols, no .data/.bss (the image is read-only
flash), and it fits its page. docs/writing-modules.md explains why.
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
    "esp32":   dict(prefix="xtensa-esp32-elf", fixed=True, code_base=0x40300000, data_base=0x3F700000, rust_target="xtensa-esp32-espidf"),
    "esp32s3": dict(prefix="xtensa-esp32s3-elf", fixed=True, code_base=0x43000000, data_base=0x3D000000, rust_target="xtensa-esp32s3-espidf"),
    "esp32c6": dict(prefix="riscv32-esp-elf", fixed=False, code_base=0, data_base=0,
                    rust_target="riscv32imac-unknown-none-elf"),
}


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


def manifest_with(base, autostart=None, flash_slots=(), name=None, tag=None, port=0, stream=0):
    """The manifest of a module (include/xip.h, module_manifest): the catalog of its API.md in compact CBOR, which the
    console needs instead of IDs compiled into it, as `mesh-api-gen --out-manifest` writes it (`scripts/gen.sh`), plus
    what only this build knows:

      6: start request     (the module starts at boot: text, may be empty)
      7: flash slot indices (flash writes are declared)
      9: port              (an integer: the datagram port whose packets the module receives and answers; the runtime binds it at start. 1: the console)
      10: stream port      (an integer: TCP port PORT_NET + n; connections arrive as socket events the module reads and writes)

    `base` is the generated map (keys 1..5: name, component number, methods, records, ...). A module without an API.md has none: its manifest is
    then `{1: name, 2: component number}` from `name` and `tag`, because the loader finds a module by scanning and reads its name from here.
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
    if flash_slots:
        extra += cbor(7) + cbor(sorted(set(flash_slots)))
        count += 1
    if port:
        extra += cbor(9) + cbor(port)
        count += 1
    if stream:
        extra += cbor(10) + cbor(stream)
        count += 1
    return bytes([0xa0 + count]) + base[1:] + extra


def manifest_beside(api_path, autostart=None, flash_slots=(), port=0, stream=0):
    """The manifest of the module whose API.md is `api_path`: the checked-in file beside it, `API.manifest`."""
    path = os.path.splitext(api_path)[0] + ".manifest"
    if not os.path.exists(path):
        sys.exit(f"{path} is missing: run scripts/gen.sh (it needs mesh-api-gen from ssh-mesh)")
    with open(path, "rb") as f:
        return manifest_with(f.read(), autostart, flash_slots, port=port, stream=stream)


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


def cargo_link(cfg, cargo, window, link_script, tmp):
    """Return link(data_vma) -> elf for a Rust module: `cargo rustc` of the bin with the module linker script.

    cargo = (manifest, bin, features). Needs the Rust toolchain with the ESP targets on the PATH (rust-src for
    -Zbuild-std). Xtensa Rust cannot produce PIC: it is linked at the page's fixed address (like C).
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


MAX_IMAGE = 0x10000   # one flash page for now (the loader scans in pages and an image may span several; the build lays out one each)


def build(chip, out, name=None, tag=None, offset=0, stack=8192, state=0, keep_elf=None, cargo=None,
          autostart=None, flash_slots=(), port=0, stream=0):
    """`offset` matters on the chips that run a module at a fixed address (Xtensa): the image is linked for code_base + offset and must be
    installed at that offset of the module partition (the loader checks); elsewhere it is position independent and goes anywhere on a page."""
    cfg = CHIPS[chip]
    tool = cfg["prefix"] + "-"
    window = (cfg["code_base"] + offset) if cfg["fixed"] else 0          # image start
    data_window = (cfg["data_base"] + offset) if cfg["fixed"] else 0
    link_script = os.path.join(ROOT, "modules", "common", "link-fixed.x" if cfg["fixed"] else "link.x")
    with tempfile.TemporaryDirectory() as tmp:
        elf = os.path.join(tmp, "module.elf")
        link = cargo_link(cfg, cargo, window, link_script, tmp)

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
        api = api_beside(None, cargo)
        if api:
            manifest = manifest_beside(api, autostart, flash_slots, port, stream)
            api_name, api_tag = manifest_identity(open(os.path.splitext(api)[0] + ".manifest", "rb").read())
            name, tag = name or api_name, tag or api_tag
            if tag != api_tag:
                sys.exit(f"--tag {tag} is not the component number {api_tag} of its API.md")
        else:
            manifest = manifest_with(b"", autostart, flash_slots, name, tag, port, stream)
        if not tag or not name:
            sys.exit("a module needs a name and a component number (its API.md, or --name and --tag)")
        image = dmod.pack(code, entry - window, tag, code_vma=window, data_vma=data_window, stack_bytes=stack,
                          fixed_vma=cfg["fixed"], state_bytes=state, manifest=manifest)
    if len(image) > MAX_IMAGE:
        sys.exit(f"image is {len(image)} bytes, the limit is {MAX_IMAGE}")
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, "wb") as f:
        f.write(image)
    print(f"{out}: {len(image)} bytes, module {name} (component {tag}), offset 0x{offset:x}, entry +0x{entry - window:x}")
    return image


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--chip", required=True, choices=sorted(CHIPS))
    ap.add_argument("--out", required=True)
    ap.add_argument("--name", help="the module's name (default: from its API.md)")
    ap.add_argument("--tag", type=int, help="the module's component number (default: from its API.md)")
    ap.add_argument("--offset", type=lambda v: int(v, 0), default=0,
                    help="Xtensa: the offset in the module partition the image will be installed at (a multiple of 0x10000); it is linked for it")
    ap.add_argument("--stack", type=int, default=8192, help="stack the deepest handler needs, bytes")
    ap.add_argument("--state", type=int, default=0, help="size of the zeroed state block passed to every call, bytes (<= 4096)")
    ap.add_argument("--autostart", nargs="?", const="", default=None, metavar="REQUEST",
                    help="start this module at boot (recorded in its manifest) with REQUEST as its start request")
    ap.add_argument("--port", type=int, default=0, help="the port the module receives frames from and answers on (manifest key 9; 1: the console)")
    ap.add_argument("--stream", type=int, default=0, help="the stream port: TCP port PORT_NET + N, connections arrive as socket events (manifest key 10)")
    ap.add_argument("--flash-slots", default="", metavar="INDICES",
                    help="comma-separated indices this image may erase/write (manifest key 7)")
    ap.add_argument("--elf", help="also keep the linked ELF here (objdump -d)")
    ap.add_argument("--cargo", required=True, metavar="MANIFEST:BIN[:FEATURES]",
                    help="the Rust module to build: the bin BIN of the crate at MANIFEST (e.g. modules/mod_lora/Cargo.toml:mod_lora:flat-image)")
    a = ap.parse_args()
    parts = a.cargo.split(":")
    if len(parts) < 2:
        ap.error("--cargo wants MANIFEST:BIN[:FEATURES]")
    cargo = (os.path.abspath(parts[0]), parts[1], parts[2] if len(parts) > 2 else "")
    try:
        flash_slots = tuple(int(item, 0) for item in a.flash_slots.split(",") if item)
    except ValueError:
        ap.error("--flash-slots requires comma-separated integer indices")
    build(a.chip, a.out, a.name, a.tag, a.offset, a.stack, a.state, keep_elf=a.elf,
          cargo=cargo, autostart=a.autostart, flash_slots=flash_slots, port=a.port, stream=a.stream)


if __name__ == "__main__":
    main()
