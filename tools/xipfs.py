#!/usr/bin/env python3
"""Offline reference reader/builder for flash-format.md. Never use while xip-host owns the file."""
import argparse
import hashlib
import json
import struct
from pathlib import Path

PAGE = 65536
BASE = 0x10000
FRONT = 0x180000

def u32(data, at):
    return struct.unpack_from('<I', data, at)[0]

def records(data, start, end):
    while start + 4 <= end:
        kind = data[start]
        if kind == 255:
            return start
        n = struct.unpack_from('<H', data, start + 2)[0]
        if not kind & 128 or not 1 <= kind & 15 <= 5 or start + 4 + ((n + 3) & ~3) > end:
            return end
        yield start, kind, n
        start += 4 + ((n + 3) & ~3)

def image_size(data, at, end):
    if at + 36 > end or not 1 <= data[at+1] <= 16 or u32(data, at+32) != 0xabcd5432:
        return 0
    pos = at + 24
    for _ in range(data[at+1]):
        if pos + 8 > end:
            return 0
        pos += 8 + u32(data, pos+4)
        if pos > end:
            return 0
    pos = ((pos-at) | 15) + 1 + (32 if data[at+23] else 0)
    return pos if at + pos <= end else 0

def extents(data):
    at = BASE
    while at < len(data):
        kind = data[at]
        n = image_size(data, at, min(len(data), at+255*PAGE)) if kind == 0xe9 else u32(data, at+4) if kind in (0, ord('M'), 0x4f, ord('D'), ord('X')) else 0
        if kind in (ord('M'), 0x4f) and n <= 64:
            n = 0
        if kind in (ord('D'), ord('X')) and n != PAGE:
            n = 0
        if not n or n > len(data)-at:
            at += PAGE
            continue
        size = ((n+PAGE-1)//PAGE)*PAGE
        if kind in (ord('D'), ord('X'), ord('M'), 0xe9):
            yield at, size, kind, 16 if kind in (ord('D'), ord('X')) else (n+3)&~3
        at += size

def files(data):
    result = {}
    for at, size, kind, body in extents(data):
        if kind == ord('X'):
            continue
        for off, typ, n in records(data, at+body+(4 if at == BASE and kind == 0xe9 else 0), at+size):
            if typ & 0x50 != 0x10 or not 1 <= typ & 15 <= 3 or n < 12:
                continue
            name, version = struct.unpack_from('<8sI', data, off+4)
            item = dict(name=name.hex(), version=version, size=n-12, kind=typ&15, cache=not bool(typ&32), offset=off-BASE)
            old = result.get(name)
            if old is None or (version, off-BASE) > (old['version'], old['offset']):
                result[name] = item
    return result

def initialize(path, mb):
    data = bytearray(b'\xff') * (mb*1024*1024)
    table = bytearray()
    for kind, sub, start, size, name in [(1,2,0x9000,0x4000,'nvs'), (1,0x40,0xd000,0x2000,'wlstate'), (1,1,0xf000,0x1000,'phy_init'), (0,0,BASE,len(data)-BASE,'xip')]:
        table += struct.pack('<HBBII16sI', 0x50aa, kind, sub, start, size, name.encode(), 0)
    table += b'\xeb\xeb' + b'\xff'*14 + hashlib.md5(table).digest()
    data[0x8000:0x8000+len(table)] = table
    with path.open('xb') as f:
        f.write(data)

def put(path, name, content, version, kind, cache):
    data = bytearray(path.read_bytes())
    name = name.encode()
    if not 1 <= len(name) <= 8 or len(content) > 65504 or not 0 < version <= 0xffffffff:
        raise ValueError('name, version or content size outside format limits')
    name = name.ljust(8, b'\0')
    old = files(data).get(name)
    if old and version <= old['version']:
        raise ValueError('version must increase')
    # One page per fixture object keeps the independent writer deliberately small.
    used = set()
    for at, size, _, _ in extents(data):
        used.update(range(at, at+size, PAGE))
    page = next((at for at in range(len(data)-PAGE, BASE+FRONT-1, -PAGE) if at not in used and data[at:at+PAGE] == b'\xff'*PAGE), None)
    if page is None:
        raise ValueError('no erased data page')
    data[page:page+16] = struct.pack('<4sIII', b'D\xff\xff\xff', PAGE, 0xffffffff, 0xffffffff)
    payload = name + struct.pack('<I', version) + content
    at = page+16
    data[at:at+4] = struct.pack('<BBH', 0x90 | (0 if cache else 32) | kind, 255, len(payload))
    data[at+4:at+4+len(payload)] = payload
    if old:
        data[BASE+old['offset']] &= ~16
    # Invalidate any old acceleration snapshot; records are authoritative.
    for at, _, kind, _ in list(extents(data)):
        if kind == ord('X'):
            data[at] = 0
    path.write_bytes(data)

def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='command', required=True)
    init = sub.add_parser('init'); init.add_argument('image', type=Path); init.add_argument('--size', type=int, choices=(2,4,8,16), default=4, help='MiB')
    ls = sub.add_parser('list'); ls.add_argument('image', type=Path)
    read = sub.add_parser('read'); read.add_argument('image', type=Path); read.add_argument('name'); read.add_argument('output', type=Path)
    write = sub.add_parser('put'); write.add_argument('image', type=Path); write.add_argument('name'); write.add_argument('content', type=Path); write.add_argument('--version', type=int, default=1); write.add_argument('--kind', type=int, choices=(1,2,3), default=1); write.add_argument('--cache', action='store_true')
    args = p.parse_args()
    if args.command == 'init':
        initialize(args.image, args.size)
    elif args.command == 'put':
        put(args.image, args.name, args.content.read_bytes(), args.version, args.kind, args.cache)
    else:
        data = args.image.read_bytes()
        if args.command == 'list':
            print(json.dumps(sorted(files(data).values(), key=lambda f:f['name']), indent=2))
        else:
            f = files(data)[args.name.encode().ljust(8,b'\0')]
            at = BASE + f['offset'] + 16
            args.output.write_bytes(data[at:at+f['size']])

if __name__ == '__main__':
    main()
