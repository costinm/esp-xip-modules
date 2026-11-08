#!/usr/bin/env python3
"""Prepare a front-runtime image for a serial initial install or recovery."""
import argparse
from pathlib import Path

from xipfs import image_size


def prepare(image, marker):
    if marker not in (0xfe, 0xfc):
        raise ValueError('expected initial (FE) or recovery (FC) marker')
    size = image_size(image, 0, len(image)) if image and image[0] == 0xe9 else 0
    if not size or len(image) != size:
        raise ValueError('expected a raw ESP runtime image without a marker or trailing bytes')
    if ((size + 3) & ~3) + 4 > 0x180000:
        raise ValueError('front runtime must leave room for its marker within RESERVE')
    return image + b'\xff' * (-size % 4) + bytes([marker, 255, 255, 255])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--marker', choices=['initial', 'recovery'], required=True)
    args = parser.parse_args()
    args.output.write_bytes(prepare(args.image.read_bytes(), 0xfe if args.marker == 'initial' else 0xfc))


if __name__ == '__main__':
    main()
