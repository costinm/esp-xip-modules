#!/usr/bin/env python3
"""Send an image to the mod_flash module (TCP 5227) as an UNSIGNED package (the device needs `nv flash.auth=unsigned`).
  flash_upload.py HOST main FILE | module NAME FILE | loader FILE | stage2 FILE | flash 0xADDR FILE   [--chip esp32c6] [--version N] [--dry]
The package format is the loader project's (docs/development.md, "Formats"); key and signature are zero."""
import argparse, hashlib, socket, struct, sys

CPU = {"esp32": 0, "esp32s3": 9, "esp32c6": 13}
TARGET = {"main": 1, "loader": 2, "stage2": 3, "module": 4, "flash": 5}
BLOCK = 4096


def package(image: bytes, target: int, name: bytes, cpu: int, version: int = 1, dry: bool = False) -> bytes:
    blocks = [image[i:i + BLOCK] for i in range(0, len(image), BLOCK)]
    table = b"".join(hashlib.sha256(b).digest()[:8] for b in blocks)
    h = bytearray(216)
    h[0:4] = b"WLH1"
    h[4], h[5], h[6] = 1, cpu, target
    h[8:20] = struct.pack("<IIHH", version, len(image), BLOCK, len(blocks))
    h[20:52] = hashlib.sha256(image).digest()
    h[52:84] = hashlib.sha256(table).digest()
    h[84] = len(name)
    h[88:88 + len(name)] = name
    transport = b"WLU1" + bytes([1, 2 if dry else 0, 0, 0]) + bytes(8)
    return transport + bytes(65) + bytes(64) + bytes(h) + table + image


def send(host: str, data: bytes, port: int = 5227) -> str:
    s = socket.create_connection((host, port), 10)
    s.settimeout(60)
    reply = b""
    try:
        s.sendall(data)
        s.shutdown(socket.SHUT_WR)
    except (ConnectionResetError, BrokenPipeError):
        pass    # the device refused the package and closed: its reply is still in the receive buffer
    try:
        while not reply.endswith(b"\n"):
            chunk = s.recv(100)
            if not chunk:
                break
            reply += chunk
    except ConnectionResetError:
        pass
    return reply.decode().strip() or "ERR no reply (connection closed)"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("host")
    ap.add_argument("kind", choices=sorted(TARGET))
    ap.add_argument("args", nargs="+", help="[NAME|ADDRESS] FILE")
    ap.add_argument("--chip", default="esp32c6", choices=sorted(CPU))
    ap.add_argument("--version", type=int, default=1)
    ap.add_argument("--dry", action="store_true")
    a = ap.parse_args()
    name = b""
    if a.kind in ("module", "flash"):
        if len(a.args) != 2:
            sys.exit(f"{a.kind} needs NAME_OR_ADDRESS FILE")
        name = (f"{int(a.args[0], 0):08x}" if a.kind == "flash" else a.args[0]).encode()
    elif len(a.args) != 1:
        sys.exit(f"{a.kind} needs FILE")
    image = open(a.args[-1], "rb").read()
    reply = send(a.host, package(image, TARGET[a.kind], name, CPU[a.chip], a.version, a.dry))
    print(reply)
    sys.exit(0 if reply.startswith("OK") else 1)


if __name__ == "__main__":
    main()
