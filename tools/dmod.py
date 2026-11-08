"""The module image header (DMOD). Constants are read from include/xip.h (generated from
crates/xip-abi/src/host.rs), so the packer, the tests and the device cannot disagree."""
import os
import re
import struct

with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "include", "xip.h")) as _f:
    _CONST = {m.group(1): int(m.group(2), 0) for m in re.finditer(r"^#define XIP_(\w+) (0x[0-9a-fA-F]+|\d+)u?\b", _f.read(), re.M)}

ABI_VERSION = _CONST["ABI_VERSION"]
KIND = _CONST["DMOD_KIND_MODULE"]
HEADER_SIZE = _CONST["DMOD_HEADER_SIZE"]
FLAG_FIXED_VMA = _CONST["DMOD_FLAG_FIXED_VMA"]
FLAG_STATIC_RAM = _CONST["DMOD_FLAG_STATIC_RAM"]
STATIC_SLOT_BYTES = _CONST["DMOD_STATIC_SLOT_BYTES"]
STATIC_SLOTS = _CONST["DMOD_STATIC_SLOTS"]
STATIC_BASES = {"esp32": _CONST["DMOD_STATIC_ESP32_BASE"],
                "esp32c6": _CONST["DMOD_STATIC_ESP32C6_BASE"],
                "esp32s3": _CONST["DMOD_STATIC_ESP32S3_BASE"]}

# kind u8 ('M'), abi u8, header_size u16, image_size u32 (the 8-byte lead of every extent), entry u32, reserved u32, tag u16, reserved u16,
# code_vma u32, data_vma u32, stack_bytes u32, required_features u32, flags u32, state_bytes u32,
# manifest_offset u32, manifest_size u32, static_addr u32, 8 reserved bytes
_FMT = struct.Struct("<BBHIIIHHIIIIIIIII8x")
assert _FMT.size == HEADER_SIZE


def pack(code: bytes, entry_offset: int, tag: int, code_vma=0, data_vma=0, stack_bytes=0,
         required_features=0, fixed_vma=False, state_bytes=0, manifest=b"", *, static_addr) -> bytes:
    """Header + code (+ manifest), each padded to a multiple of 4. `entry_offset` counts from the start of the image."""
    if not any(base <= static_addr < base + STATIC_SLOTS * STATIC_SLOT_BYTES
               and (static_addr - base) % STATIC_SLOT_BYTES == 0 for base in STATIC_BASES.values()):
        raise ValueError("static_addr must name an aligned cell in a platform module RAM arena")
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
    header = _FMT.pack(KIND, ABI_VERSION, HEADER_SIZE, size, entry_offset, 0, tag, 0, code_vma, data_vma,
                       stack_bytes, required_features, FLAG_STATIC_RAM | (FLAG_FIXED_VMA if fixed_vma else 0), state_bytes,
                       manifest_offset, len(manifest), static_addr)
    return header + code + padded


def unpack(image: bytes) -> dict:
    (kind, abi, hsize, size, entry, _r1, tag, _r, code_vma, data_vma, stack, features,
     flags, state, manifest_offset, manifest_size, static_addr) = _FMT.unpack(image[:HEADER_SIZE])
    return dict(kind=kind, abi_version=abi, header_size=hsize, entry_offset=entry, image_size=size, tag=tag,
                code_vma=code_vma, data_vma=data_vma, stack_bytes=stack, required_features=features, flags=flags, state_bytes=state,
                manifest_offset=manifest_offset, manifest_size=manifest_size, static_addr=static_addr)


def manifest_of(image: bytes) -> bytes:
    """The manifest bytes of an image (b"" if it has none)."""
    h = unpack(image)
    return image[h["manifest_offset"]:h["manifest_offset"] + h["manifest_size"]]


def describe_manifest(image: bytes) -> dict:
    """The manifest of an image as {key: value} (the keys mkmod writes: 1 name, 2 component, 6 autostart, 8 version, 9 port,
    10 stream, 11 components, 13 wire components as [[id, version], ...])."""
    from mkmod import _cbor_item   # lazy: mkmod imports this module
    base = manifest_of(image)
    if not base:
        return {}
    fields, _ = _cbor_item(base, 0)
    return fields


if __name__ == "__main__":
    import sys
    with open(sys.argv[1], "rb") as f:
        for key, value in sorted(describe_manifest(f.read()).items()):
            print(f"{key}: {value}")
