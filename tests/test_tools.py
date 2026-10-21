"""Host tests of the tools and of the pieces that must agree with each other. No board needed."""
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dmod  # noqa: E402
import mkmod  # noqa: E402
import flash_upload  # noqa: E402


def read(*path):
    with open(os.path.join(ROOT, *path)) as f:
        return f.read()


class Abi(unittest.TestCase):
    def test_python_and_c_agree_on_the_image_header(self):
        header = read("include", "xip.h")
        self.assertIn(f"#define XIP_DMOD_MAGIC {dmod.MAGIC}", header)
        self.assertIn(f"#define XIP_ABI_VERSION {dmod.ABI_VERSION}", header)
        self.assertEqual(dmod._FMT.size, 64)


class Dmod(unittest.TestCase):
    def test_roundtrip(self):
        image = dmod.pack(b"\x01\x02\x03\x04" * 20, 0x44, 7, code_vma=0x40300000, data_vma=0x3F700000, stack_bytes=8192,
                          required_features=3, fixed_vma=True, static_addr=dmod.STATIC_BASES["esp32"])
        h = dmod.unpack(image)
        self.assertEqual((h["tag"], h["entry_offset"], h["image_size"], h["flags"]), (7, 0x44, 64 + 80, 3))
        self.assertEqual(len(image) % 4, 0)

    def test_rejects_bad_entry(self):
        for entry in (0x10, 0x40 + 100, 0x45):
            with self.assertRaises(ValueError):
                dmod.pack(b"\0" * 100, entry, 1, static_addr=dmod.STATIC_BASES["esp32c6"])

    def test_static_ram_cell_is_explicit_and_serialized(self):
        address = mkmod.static_address("esp32c6", 4 * 65536)
        image = dmod.pack(b"\0" * 32, 0x44, 2, static_addr=address)
        header = dmod.unpack(image)
        self.assertEqual(header["static_addr"], address)
        self.assertEqual(header["flags"], dmod.FLAG_STATIC_RAM)
        self.assertEqual(int.from_bytes(image[52:56], "little"), address)
        self.assertEqual(image[56:64], b"\0" * 8)
        with self.assertRaises(TypeError):
            dmod.pack(b"\0" * 32, 0x44, 2)
        for bad in (0, address + 1, dmod.STATIC_BASES["esp32c6"] - 16,
                    dmod.STATIC_BASES["esp32c6"] + dmod.STATIC_SLOTS * dmod.STATIC_SLOT_BYTES):
            with self.assertRaises(ValueError):
                dmod.pack(b"\0" * 32, 0x44, 2, static_addr=bad)

    def test_platform_cell_derivation_and_slot_limit(self):
        for chip, base in dmod.STATIC_BASES.items():
            self.assertEqual(mkmod.static_address(chip, 0), base)
            self.assertEqual(mkmod.static_address(chip, 15 * 65536), base + 15 * 16)
            for bad_offset in (-65536, 1, 65535, 16 * 65536):
                with self.assertRaises(ValueError):
                    mkmod.static_address(chip, bad_offset)


class Slots(unittest.TestCase):
    def test_a_module_without_an_api_gets_its_name_and_number(self):
        m, _ = cbor_decode(mkmod.manifest_with(b"", name="nan", tag=46))
        self.assertEqual((m[1], m[2]), ("nan", 46))
        with self.assertRaises(SystemExit):
            mkmod.manifest_with(b"")        # nothing to name it by

    def test_the_identity_of_a_generated_manifest(self):
        name, tag = mkmod.manifest_identity(bytes.fromhex(Manifest.DEMO))
        m, _ = cbor_decode(bytes.fromhex(Manifest.DEMO))
        self.assertEqual((name, tag), (m[1], m[2]))

    def test_chip_windows_match_the_loader(self):
        loader = read("components", "xip_loader", "src", "loader.c")
        for chip, ifdef in (("esp32s3", "#if CONFIG_IDF_TARGET_ESP32S3\n#define XIP_FIXED"),
                            ("esp32", "#elif CONFIG_IDF_TARGET_ESP32\n#define XIP_FIXED")):
            block = loader[loader.index(ifdef):]
            code = int(re.search(r"XIP_CODE_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
            data = int(re.search(r"XIP_DATA_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
            self.assertEqual((code, data), (mkmod.CHIPS[chip]["code_base"], mkmod.CHIPS[chip]["data_base"]), chip)


class Packages(unittest.TestCase):
    """The package format of the upload port (docs/development.md, "Formats"): key (65) | signature (64) | header (216) | block table | image.
    The committed vectors were made once by the frozen loader tools; mod_flash's own tests verify and install them."""
    DATA = os.path.join(ROOT, "tests", "data")

    def parts(self, pkg):
        key, sig, header = pkg[:65], pkg[65:129], pkg[129:345]
        self.assertEqual(header[:4], b"WLH1")
        target, version, size, bsize, count = header[6], int.from_bytes(header[8:12], "little"), int.from_bytes(header[12:16], "little"), \
            int.from_bytes(header[16:18], "little"), int.from_bytes(header[18:20], "little")
        table = pkg[345:345 + 8 * count]
        image = pkg[345 + 8 * count:]
        self.assertEqual((bsize, len(image), count), (4096, size, -(-size // 4096)))
        self.assertEqual(header[20:52], hashlib.sha256(image).digest())
        self.assertEqual(header[52:84], hashlib.sha256(table).digest())
        for i in range(count):
            self.assertEqual(table[8 * i:8 * i + 8], hashlib.sha256(image[4096 * i:4096 * (i + 1)]).digest()[:8])
        return key, sig, header[84], header[88:88 + header[84]], target, version, image

    def test_vectors_of_the_signed_package(self):
        key, sig, _, name, target, version, image = self.parts(open(os.path.join(self.DATA, "module-lora-esp32c6.pkg"), "rb").read())
        self.assertEqual((key[0], name, target), (4, b"lora", 4))
        self.assertNotEqual(sig, bytes(64))
        _, _, _, _, target, version, image = self.parts(open(os.path.join(self.DATA, "loader-esp32c6.pkg"), "rb").read())
        self.assertEqual((target, version, image[0]), (2, 9, 0xe9))

    def test_the_unsigned_package_of_flash_upload_has_the_same_layout(self):
        image = dmod.pack(os.urandom(5000), 0x44, 2, static_addr=dmod.STATIC_BASES["esp32c6"])
        key, sig, _, name, target, version, body = self.parts(flash_upload.package(image, 4, b"test", 13, 9)[16:])
        self.assertEqual((key, sig, name, target, version, body), (bytes(65), bytes(64), b"test", 4, 9, image))
        self.assertEqual(flash_upload.package(image, 4, b"test", 13, dry=True)[5], 2)


def cbor_decode(data, pos=0):
    """Just enough CBOR to read a manifest back: (value, next position)."""
    major, info = data[pos] >> 5, data[pos] & 31
    pos += 1
    n = info
    if info == 24:
        n, pos = data[pos], pos + 1
    elif info == 25:
        n, pos = int.from_bytes(data[pos:pos + 2], "big"), pos + 2
    if major == 0:
        return n, pos
    if major == 3:
        return data[pos:pos + n].decode(), pos + n
    if major == 4:
        out = []
        for _ in range(n):
            v, pos = cbor_decode(data, pos)
            out.append(v)
        return out, pos
    if major == 5:
        out = {}
        for _ in range(n):
            k, pos = cbor_decode(data, pos)
            out[k], pos = cbor_decode(data, pos)
        return out, pos
    raise ValueError(major)


class Manifest(unittest.TestCase):
    DEMO = open(os.path.join(ROOT, "tests", "data", "manifest-demo.hex")).read().strip()

    def test_the_manifest_is_the_one_ssh_mesh_writes(self):
        """mkmod adds what only the build knows to the manifest mesh-api-gen wrote (crates/mesh-api-gen/tests/data there)."""
        data = os.path.join(ROOT, "tests", "data")
        base = bytes.fromhex(self.DEMO)
        self.assertEqual(mkmod.manifest_with(base), base)
        self.assertEqual(mkmod.manifest_with(base, autostart="").hex(),
                         open(os.path.join(data, "manifest-demo-autostart.hex")).read().strip())
        m, _ = cbor_decode(mkmod.manifest_with(base, autostart="serve"))
        self.assertEqual(m[6], "serve")
        self.assertEqual(m[4][0][0], "Row")

    def test_the_manifest_sits_after_the_code_inside_the_image(self):
        m = mkmod.manifest_with(bytes.fromhex(self.DEMO))
        image = dmod.pack(b"\x01\x02\x03\x04" * 5, 0x44, 7, manifest=m, static_addr=dmod.STATIC_BASES["esp32c6"])
        h = dmod.unpack(image)
        self.assertEqual(h["manifest_offset"], 64 + 20)
        self.assertEqual(h["manifest_size"], len(m))
        self.assertEqual(h["image_size"], len(image))
        self.assertEqual(dmod.manifest_of(image), m)
        self.assertEqual(len(image) % 4, 0)
        plain = dmod.unpack(dmod.pack(b"\x01\x02\x03\x04" * 2, 0x44, 7, static_addr=dmod.STATIC_BASES["esp32c6"]))
        self.assertEqual((plain["manifest_offset"], plain["manifest_size"]), (0, 0))

    def test_every_module_api_has_its_checked_in_manifest(self):
        import glob
        for api in glob.glob(os.path.join(ROOT, "modules", "*", "API.md")):
            self.assertTrue(os.path.exists(os.path.splitext(api)[0] + ".manifest"), api)

    def test_a_built_module_carries_its_manifest(self):
        image_path = os.path.join(ROOT, "target", "out", "esp32c6", "modules", "mod_rtest.dmod")
        if not os.path.exists(image_path):
            self.skipTest("no built image (scripts/build.sh)")
        m, _ = cbor_decode(dmod.manifest_of(open(image_path, "rb").read()))
        self.assertEqual((m[1], m[2]), ("test", 2))
        self.assertIn("echo", [x[2] for x in m[3]])

    def test_native_metadata_matches_the_packed_manifest_and_state(self):
        image_path = os.path.join(ROOT, "target/out/esp32c6/modules/mod_rtest.dmod")
        if not os.path.exists(image_path):
            self.skipTest("no built module image")
        with tempfile.TemporaryDirectory() as directory:
            metadata_path = os.path.join(directory, "module.json")
            subprocess.run([sys.executable, os.path.join(ROOT, "tools/build_mod.py"),
                            os.path.join(ROOT, "modules/mod_rtest"), "--host-metadata", metadata_path], check=True)
            with open(metadata_path) as source:
                metadata = json.load(source)
        with open(image_path, "rb") as source:
            image = source.read()
        self.assertEqual(bytes(metadata["manifest"]), dmod.manifest_of(image))
        self.assertEqual(metadata["state_bytes"], dmod.unpack(image)["state_bytes"])


class ConsoleAnswer(unittest.TestCase):
    """tools/console.py: the answer to a command is what is not an ESP-IDF log line."""

    def test_log_lines_between_the_reply_are_dropped(self):
        import console

        class Ser:
            def __init__(self, chunks):
                self.chunks, self.sent = list(chunks), b""

            def reset_input_buffer(self): pass
            def write(self, b): self.sent += b
            def read(self, n): return self.chunks.pop(0) if self.chunks else b""

        c = console.Console("unused")
        c.ser = Ser([b"W (1234) xip: test: log line\r\n", b"name=test tag=2 installed=true\r\n",
                     b"W (1240) main: test event 0x7eff: pass=1\r\n"])
        self.assertEqual(c.command("status", timeout=3, idle=0.01), "name=test tag=2 installed=true\n")
        self.assertEqual(c.ser.sent, b"status\n")
        c.ser = Ser([])
        self.assertEqual(c.command("status", timeout=0.2, idle=0.01), "no reply")

    def test_a_board_is_discovered_by_asking_its_console(self):
        import console

        class Ser:
            sent = b""
            chunks = [b"", b"", b'{1:{4:"main"},4:[{1:"console",2:20,3:true,4:true},{1:"test",2:2,3:true,4:false}]}\r\n']

            def reset_input_buffer(self): pass
            def write(self, b): self.sent += b
            def read(self, n): return self.chunks.pop(0) if self.chunks else b""
            def close(self): pass

        original = console.Console._open_serial
        console.Console._open_serial = staticmethod(lambda path: Ser())
        try:
            d = console.probe("/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_10:BD:A3:AC:5A:20-if00", seconds=3)
        finally:
            console.Console._open_serial = original
        self.assertEqual(d["mac"], "10:bd:a3:ac:5a:20")
        self.assertEqual([(m["name"], m["running"]) for m in d["modules"]], [("console", True), ("test", False)])


if __name__ == "__main__":
    unittest.main()
