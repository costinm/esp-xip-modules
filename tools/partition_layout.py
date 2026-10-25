#!/usr/bin/env python3
"""Read the selected/installed ESP partition layout and check packed module extents."""
import argparse
import csv
import hashlib
import struct
from pathlib import Path


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
    if 'main' not in result or 'loader' not in result:
        raise ValueError('no main/loader partition')
    return result


def validate_modules(layout, listing, directory):
    base, capacity = layout['data']
    if base % 65536 or capacity % 65536:
        raise ValueError('module partition must use 64 KiB pages')
    extents = []
    for line in Path(listing).read_text().splitlines():
        if not line.strip():
            continue
        name, offset, _slot = line.split()
        offset = int(offset, 0)
        size = (Path(directory) / (name + '.dmod')).stat().st_size
        rounded = (size + 65535) // 65536 * 65536
        if size <= 64 or offset < 0 or offset % 65536 or offset + rounded > capacity:
            raise ValueError(f'{name} does not fit module partition')
        extents.append((offset, offset + rounded, name))
    extents.sort()
    for previous, current in zip(extents, extents[1:]):
        if previous[1] > current[0]:
            raise ValueError(f'overlapping module extents {previous[2]}, {current[2]}')


# The two-sector WLS1 journal occupies the end of the recovery partition.
LOADER_STATE_BYTES = 8192


def validate_image(layout, label, image, reserved=0):
    if Path(image).stat().st_size > layout[label][1] - reserved:
        raise ValueError(f'{label} image exceeds installed/selected partition capacity')


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('table', type=Path)
    ap.add_argument('--modules', type=Path)
    ap.add_argument('--images', type=Path)
    ap.add_argument('--main-image', type=Path)
    ap.add_argument('--loader-image', type=Path)
    args = ap.parse_args()
    layout = read_layout(args.table)
    if args.main_image:
        validate_image(layout, 'main', args.main_image)
    if args.loader_image:
        validate_image(layout, 'loader', args.loader_image, LOADER_STATE_BYTES)
    if args.modules:
        if not args.images:
            ap.error('--modules requires --images')
        validate_modules(layout, args.modules, args.images)
    main_offset, main_size = layout['main']
    data_offset, data_size = layout.get('data', (0, 0))
    print(main_offset, main_size, data_offset, data_size)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError) as err:
        raise SystemExit(str(err))
