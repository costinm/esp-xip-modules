"""Host tests of the tools and of the pieces that must agree with each other. No board needed."""
import hashlib
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
import loader_tools  # noqa: E402,F401  (esp-wifi-loader's tools)
import sign  # noqa: E402
import wl_format as wf  # noqa: E402


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
                          required_features=3, fixed_vma=True)
        h = dmod.unpack(image)
        self.assertEqual((h["tag"], h["entry_offset"], h["image_size"], h["flags"]), (7, 0x44, 64 + 80, 1))
        self.assertEqual(len(image) % 4, 0)

    def test_rejects_bad_entry(self):
        for entry in (0x10, 0x40 + 100, 0x45):
            with self.assertRaises(ValueError):
                dmod.pack(b"\0" * 100, entry, 1)


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


class Signing(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.dir = tempfile.mkdtemp()
        cls.key = os.path.join(cls.dir, "k.pem")
        sign.keygen(cls.key)

    def test_module_package_layout_and_signature(self):
        image = dmod.pack(os.urandom(5000), 0x44, 2)
        pkg = sign.make_package(self.key, "esp32c6", 9, "test", image, "module")
        pub, sig, hdr, table, body = wf.split_package(pkg)
        self.assertEqual(body, image)
        h = wf.unpack_header(hdr)
        self.assertEqual((h["target"], h["fqdn"], h["version"]), (wf.TARGETS["module"], "test", 9))
        self.assertEqual(h["image_sha256"], hashlib.sha256(image).digest())
        if shutil.which("openssl"):
            self.assertTrue(self.verify(pub, sig, hdr))
            self.assertFalse(self.verify(pub, sig, hdr[:-1] + b"\x01"))

    @staticmethod
    def verify(pub, sig, data):
        def der(x):
            b = x.to_bytes(32, "big").lstrip(b"\0") or b"\0"
            if b[0] & 0x80:
                b = b"\0" + b
            return b"\x02" + bytes([len(b)]) + b
        body = der(int.from_bytes(sig[:32], "big")) + der(int.from_bytes(sig[32:], "big"))
        with tempfile.TemporaryDirectory() as d:
            open(f"{d}/k.der", "wb").write(bytes.fromhex("3059301306072a8648ce3d020106082a8648ce3d030107034200") + pub)
            open(f"{d}/s.der", "wb").write(b"\x30" + bytes([len(body)]) + body)
            open(f"{d}/h.bin", "wb").write(hashlib.sha256(data).digest())
            r = subprocess.run(["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", f"{d}/k.der", "-keyform", "DER",
                                "-in", f"{d}/h.bin", "-sigfile", f"{d}/s.der"], capture_output=True)
            return r.returncode == 0

    def test_main_and_module_packages_share_one_format(self):
        main = sign.make_package(self.key, "esp32c6", 1, "costinm/esp-xip-modules:latest", b"\xe9" + bytes(5000), "main")
        self.assertEqual(wf.unpack_header(wf.split_package(main)[2])["target"], 1)


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

    def test_flash_slot_declaration_is_in_manifest(self):
        base = bytes.fromhex(self.DEMO)
        m, _ = cbor_decode(mkmod.manifest_with(base, flash_slots=(3, 1, 3)))
        self.assertEqual(m[7], [1, 3])
        self.assertNotIn(7, cbor_decode(mkmod.manifest_with(base))[0])

    def test_the_manifest_sits_after_the_code_inside_the_image(self):
        m = mkmod.manifest_with(bytes.fromhex(self.DEMO))
        image = dmod.pack(b"\x01\x02\x03\x04" * 5, 0x44, 7, manifest=m)
        h = dmod.unpack(image)
        self.assertEqual(h["manifest_offset"], 64 + 20)
        self.assertEqual(h["manifest_size"], len(m))
        self.assertEqual(h["image_size"], len(image))
        self.assertEqual(dmod.manifest_of(image), m)
        self.assertEqual(len(image) % 4, 0)
        plain = dmod.unpack(dmod.pack(b"\x01\x02\x03\x04" * 2, 0x44, 7))
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
