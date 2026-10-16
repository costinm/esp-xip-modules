"""The module image header (DMOD). Layout and constants come from abi/API.md, the same source as
include/xip.h, so the packer, the tests and the device cannot disagree."""
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gen_abi  # noqa: E402

_ABI = gen_abi.load()

ABI_VERSION = _ABI["abi"]["version"]
_DMOD = {n: v for g in _ABI["constants"] if g["group"] == "DMOD" for n, v, _ in g["items"]}
MAGIC = _DMOD["MAGIC"]
HEADER_SIZE = _DMOD["HEADER_SIZE"]
FLAG_FIXED_VMA = _DMOD["FLAG_FIXED_VMA"]
FEATURES = {n: v for g in _ABI["constants"] if g["group"] == "FEAT" for n, v, _ in g["items"]}

# magic u32, abi u16, header_size u16, entry u32, image_size u32, tag u16, reserved u16,
# code_vma u32, data_vma u32, stack_bytes u32, required_features u32, flags u32, state_bytes u32,
# manifest_offset u32, manifest_size u32, 12 reserved bytes
_FMT = struct.Struct("<IHHIIHHIIIIIIII12x")
assert _FMT.size == HEADER_SIZE


def pack(code: bytes, entry_offset: int, tag: int, code_vma=0, data_vma=0, stack_bytes=0,
         required_features=0, fixed_vma=False, state_bytes=0, manifest=b"") -> bytes:
    """Header + code (+ manifest), each padded to a multiple of 4. `entry_offset` counts from the start of the image."""
    if not code:
        raise ValueError("module code is empty")
    if entry_offset < HEADER_SIZE or entry_offset >= HEADER_SIZE + len(code) or entry_offset & 1:
        raise ValueError("entry offset must be inside the code, after the header")
    if not 0 < tag < 0x10000:
        raise ValueError("tag must be 1..65535")
    code += b"\xff" * (-len(code) % 4)    # image_size counts the padding: signed and header length agree
    manifest_offset = HEADER_SIZE + len(code) if manifest else 0
    padded = manifest + b"\x00" * (-len(manifest) % 4)
    size = HEADER_SIZE + len(code) + len(padded)
    header = _FMT.pack(MAGIC, ABI_VERSION, HEADER_SIZE, entry_offset, size, tag, 0, code_vma, data_vma,
                       stack_bytes, required_features, FLAG_FIXED_VMA if fixed_vma else 0, state_bytes,
                       manifest_offset, len(manifest))
    return header + code + padded


def unpack(image: bytes) -> dict:
    (magic, abi, hsize, entry, size, tag, _r, code_vma, data_vma, stack, features,
     flags, state, manifest_offset, manifest_size) = _FMT.unpack(image[:HEADER_SIZE])
    return dict(magic=magic, abi_version=abi, header_size=hsize, entry_offset=entry, image_size=size, tag=tag,
                code_vma=code_vma, data_vma=data_vma, stack_bytes=stack, required_features=features, flags=flags, state_bytes=state,
                manifest_offset=manifest_offset, manifest_size=manifest_size)


def manifest_of(image: bytes) -> bytes:
    """The manifest bytes of an image (b"" if it has none)."""
    h = unpack(image)
    return image[h["manifest_offset"]:h["manifest_offset"] + h["manifest_size"]]
