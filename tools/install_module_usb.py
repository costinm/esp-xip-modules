#!/usr/bin/env python3
"""Replace one installed module image on a board over USB (esptool), at the page it is already on.

The loader scans the module partition page by page, but the slot table the runtime was built with
(product-slots.inc) fixes the page each named module is started from: an image must be written there,
not on any free page. The page of the installed image is found by reading the partition and parsing the
DMOD headers (name = key 1 of the CBOR manifest), and the declared page comes from the build's
product-slots.inc when that is available.

  tools/install_module_usb.py PORT MODULE-NAME IMAGE.dmod [--chip esp32c6] [--verify]

Reads back the written bytes and compares them with the image (the independent check of the write).
"""
import os
import struct
import subprocess
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import dmod

PART_BASE = 0x380000
PART_SIZE = 0x80000
PAGE = 0x10000


def esptool(port, *args):
    cmd = [sys.executable, "-m", "esptool", "--chip", "esp32c6", "--port", port,
           "--baud", "460800", "--before", "default-reset", "--after", "no-reset", *args]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        sys.stderr.write(r.stdout + r.stderr)
        raise SystemExit(f"esptool {' '.join(args[:2])} failed on {port}")


def read(port, offset, size, out):
    esptool(port, "read-flash", str(offset), str(size), out)
    with open(out, "rb") as f:
        return f.read()


def name_of(page, offset):
    if page[:4] != struct.pack("<I", dmod.MAGIC):
        return None
    h = dmod.unpack(page)
    m = page[h["manifest_offset"]:h["manifest_offset"] + h["manifest_size"]]
    # the manifest is a CBOR map; name = key 1: {1: "text" ...} -> 0xa? 0x01 0x6? len bytes
    for i, b in enumerate(m):
        if b == 0x01 and 0x60 <= m[i + 1] <= 0x77:
            n = m[i + 1] - 0x60
            return m[i + 2:i + 2 + n].decode(errors="replace")
    # no manifest: the loader names the image by its offset
    return f"@{offset - PART_BASE:x}"


def scan(data):
    found = {}
    for p in range(PART_SIZE // PAGE):
        name = name_of(data[p * PAGE:(p + 1) * PAGE], PART_BASE + p * PAGE)
        if name:
            found[name] = PART_BASE + p * PAGE
    return found


def declared_page(chip):
    """name -> page offset within the partition, from the build's product slot table (None when there is none)."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "out", chip, "product-slots.inc")
    try:
        with open(path) as f:
            entries = {}
            for line in f:
                if line.startswith("{"):
                    parts = [p.strip().strip('"') for p in line.strip().rstrip(",").strip("{}").split(",")]
                    entries[parts[0]] = int(parts[2], 0)
            return entries
    except FileNotFoundError:
        return None


def main():
    argv = sys.argv[1:]
    verify = "--verify" in argv
    chip = "esp32c6"
    if "--chip" in argv:
        i = argv.index("--chip")
        chip = argv[i + 1]
        del argv[i:i + 2]
    args = [a for a in argv if a != "--verify"]
    at_page = None
    if "--at" in args:
        i = args.index("--at")
        at_page = int(args[i + 1], 0)
        del args[i:i + 2]
    if len(args) != 3:
        raise SystemExit(__doc__)
    port, module, image_path = args
    with open(image_path, "rb") as f:
        image = f.read()

    print(f"reading the module partition of {port}")
    data = read(port, PART_BASE, PART_SIZE, "/tmp/opencode/module-partition.bin")
    slots = scan(data)
    print("installed:", ", ".join(f"{k}@0x{v:x}" for k, v in sorted(slots.items(), key=lambda kv: kv[1])))
    declared = declared_page(chip)
    if declared and module in declared:
        if module in slots and slots[module] != PART_BASE + declared[module]:
            print(f"note: an image named {module} also sits at 0x{slots[module]:x}; the slot table says page 0x{declared[module]:x}")
        at = PART_BASE + declared[module]
    elif module in slots:
        at = slots[module]
    else:
        if at_page is None:
            raise SystemExit(f"{module} is not installed and the slot table does not name it; pass --at PAGE")
        at = PART_BASE + at_page
        if data[at - PART_BASE:at - PART_BASE + 4] != b"\xff\xff\xff\xff":
            raise SystemExit(f"page at {at:#x} is not erased")

    esptool(port, "write-flash", f"{at:#x}", image_path)
    print(f"wrote {len(image)} bytes at {at:#x}")
    if verify:
        back = read(port, at, len(image), "/tmp/opencode/module-readback.bin")
        if back != image:
            raise SystemExit("readback differs from the image")
        print("readback: identical")


if __name__ == "__main__":
    main()
