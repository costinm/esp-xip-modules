#!/usr/bin/env python3
"""Read the selected/installed ESP partition layout and check packed module extents."""
import argparse
import csv
import hashlib
import struct
from pathlib import Path


# Stage2 finds its two-sector state log here on every chip (WL_STATE_OFFSET in esp_wifi_loader_state.h).
WL_STATE_OFFSET = 0xd000
WL_STATE_BYTES = 0x2000
# The one partition after the first 64 KiB: the runtime at its start (at most RUNTIME_RESERVE bytes), the modules after that.
XIP_OFFSET = 0x10000
RUNTIME_RESERVE = 0x180000


def read_layout(path):
    path = Path(path)
    data = path.read_bytes()
    result = {}
    if path.suffix == '.csv':
        entries = []
        for row in csv.reader(data.decode().splitlines()):
            if not row or row[0].lstrip().startswith('#'):
                continue
            if len(row) < 5:
                raise ValueError('short CSV partition row')
            entries.append((row[0].strip(), int(row[3].strip(), 0), int(row[4].strip(), 0)))
    else:
        entries = []
        for at in range(0, len(data) - 31, 32):
            magic = struct.unpack_from('<H', data, at)[0]
            if magic == 0xffff:
                break
            if magic == 0xebeb:
                if data[at+16:at+32] != hashlib.md5(data[:at]).digest():
                    raise ValueError('partition table MD5 mismatch')
                break
            if magic != 0x50aa:
                raise ValueError(f'invalid partition entry at {at}')
            off, size = struct.unpack_from('<II', data, at + 4)
            name = data[at+12:at+28].split(b'\0', 1)[0].decode('ascii')
            entries.append((name, off, size))
    for name, off, size in entries:
        if not name or name in result or size <= 0 or off < 0 or off + size > 0x100000000:
            raise ValueError(f'invalid/duplicate partition {name}')
        result[name] = (off, size)
    ordered = sorted((off, off + size, name) for name, (off, size) in result.items())
    for previous, current in zip(ordered, ordered[1:]):
        if previous[1] > current[0]:
            raise ValueError(f'overlapping partitions {previous[2]}, {current[2]}')
    for name in ('xip', 'wlstate'):
        if name not in result:
            raise ValueError(f'no {name} partition')
    if result['xip'][0] != XIP_OFFSET or result['xip'][1] % 65536 or result['xip'][1] <= RUNTIME_RESERVE:
        raise ValueError(f'xip must start at {XIP_OFFSET:#x}, use 64 KiB pages and be larger than the runtime reserve {RUNTIME_RESERVE:#x}')
    if result['wlstate'] != (WL_STATE_OFFSET, WL_STATE_BYTES):
        raise ValueError(f'wlstate must be {WL_STATE_BYTES:#x} bytes at {WL_STATE_OFFSET:#x} (Stage2 reads it there)')
    return result


PAGE = 65536


def place_modules(layout, runtime_bytes, modules, critical=()):
    """modules: [(name, slot, size)]. Returns [(name, address, slot)] in the input order. Manual-flash layout (no scan needed to find
    anything): the runtime at the start of xip, the modules in the pages after its reserve, the critical ones (flashing and upgrade) first,
    each on consecutive pages. The module scan finds a module wherever it is, so this is only where esptool puts them."""
    base, size = layout['xip']
    end = base + size
    at = base + RUNTIME_RESERVE
    if runtime_bytes > RUNTIME_RESERVE:
        raise ValueError('runtime image exceeds the runtime reserve')
    for name, _slot, nbytes in modules:
        if nbytes <= 64:
            raise ValueError(f'{name} is not an image')
    out = {}
    for name, _slot, nbytes in [m for m in modules if m[0] in critical] + [m for m in modules if m[0] not in critical]:
        out[name] = at
        at += (nbytes + PAGE - 1) // PAGE * PAGE
        if at > end:
            raise ValueError(f'{name} does not fit the module pages')
    return [(name, out[name], slot) for name, slot, _size in modules]


def read_list(path, directory):
    """The lines `name slot` written by scripts/build.sh, with the size of each image."""
    modules = []
    for line in Path(path).read_text().splitlines():
        if line.strip():
            name, slot = line.split()
            modules.append((name, int(slot), (Path(directory) / (name + '.dmod')).stat().st_size))
    return modules


def validate_image(layout, image):
    """The runtime image goes to the start of xip and stays within its reserve."""
    data = Path(image).read_bytes()
    if len(data) > RUNTIME_RESERVE:
        raise ValueError('runtime image exceeds the runtime reserve (1.5 MiB)')
    if data[:1] != b'\xe9':
        raise ValueError('the runtime image does not start with the ESP image magic 0xE9')


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('table', type=Path)
    ap.add_argument('--modules', type=Path, help='lines `name slot` of the built modules')
    ap.add_argument('--images', type=Path, help='the directory of the .dmod images')
    ap.add_argument('--critical', type=Path, help='file listing the modules placed first after the runtime (flashing and upgrade)')
    ap.add_argument('--out', type=Path, help='write `name address slot` here (addresses are flash addresses)')
    ap.add_argument('--main-image', type=Path, help='the runtime image')
    args = ap.parse_args()
    layout = read_layout(args.table)
    if args.main_image:
        validate_image(layout, args.main_image)
    if args.modules:
        if not args.images:
            ap.error('--modules requires --images')
        runtime = args.main_image.stat().st_size if args.main_image else 0
        critical = set(args.critical.read_text().split()) if args.critical else set()
        placed = place_modules(layout, runtime, read_list(args.modules, args.images), critical)
        if args.out:
            args.out.write_text(''.join(f'{name} {address:#x} {slot}\n' for name, address, slot in placed))
    print(*layout['xip'])


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError) as err:
        raise SystemExit(str(err))
