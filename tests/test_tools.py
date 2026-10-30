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
from pathlib import Path

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dmod  # noqa: E402
import mkmod  # noqa: E402


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
        address = mkmod.static_address("esp32c6", 4)
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
            self.assertEqual(mkmod.static_address(chip, 15), base + 15 * 16)
            for bad_slot in (-1, 16, 0x10000):
                with self.assertRaises(ValueError):
                    mkmod.static_address(chip, bad_slot)

    def test_the_slot_registry_is_unique_and_covers_every_module(self):
        slots = mkmod.read_slots()
        self.assertEqual(len(set(slots.values())), len(slots))
        import glob
        for directory in glob.glob(os.path.join(ROOT, "modules", "mod_*")):
            self.assertIn(os.path.basename(directory), slots)
        with tempfile.TemporaryDirectory() as d:
            for text in ("a 1\nb 1\n", "a 1\na 2\n", "a 16\n", "a\n"):
                path = os.path.join(d, "SLOTS")
                Path(path).write_text(text)
                with self.assertRaises(ValueError):
                    mkmod.read_slots(path)

    def test_the_manifest_version_sits_behind_the_name(self):
        base = bytes.fromhex(Manifest.DEMO)
        self.assertEqual(mkmod.manifest_with(base, version=0), mkmod.manifest_with(base))      # absent is 0: nothing is written
        m, _ = cbor_decode(mkmod.manifest_with(base, autostart="x", port=3, version=300))
        self.assertEqual((m[1], m[2], m[8], m[6], m[9]), (cbor_decode(base)[0][1], cbor_decode(base)[0][2], 300, "x", 3))
        raw = mkmod.manifest_with(base, version=7)
        name = cbor_decode(base)[0][1].encode()
        at = raw.index(name) + len(name)
        self.assertEqual(raw[at:at + 2], bytes([8, 7]))     # `8: 7` right behind `1: name`
        again = mkmod.with_version(raw, 9)                  # replaced, not duplicated
        self.assertEqual(cbor_decode(again)[0][8], 9)
        self.assertEqual(len(again), len(raw))

    def test_the_hosted_components_are_manifest_key_11(self):
        base = bytes.fromhex(Manifest.DEMO)
        self.assertEqual(mkmod.manifest_with(base, components=()), mkmod.manifest_with(base))
        m, _ = cbor_decode(mkmod.manifest_with(base, port=4, version=2, components=["ble", "other"]))
        self.assertEqual((m[11], m[9], m[8]), (["ble", "other"], 4, 2))
        for bad in (["a", "b", "c", "d"], [""], ["x" * 16]):
            with self.assertRaises(SystemExit):
                mkmod.manifest_with(base, components=bad)


class Slots(unittest.TestCase):
    def test_a_module_without_an_api_gets_its_name_and_number(self):
        m, _ = cbor_decode(mkmod.manifest_with(b"", name="nan", tag=46))
        self.assertEqual((m[1], m[2]), ("nan", 46))
        with self.assertRaises(SystemExit):
            mkmod.manifest_with(b"")        # nothing to name it by

    def test_additional_envelope_components_are_manifest_key_12(self):
        base = bytes.fromhex(Manifest.DEMO)
        self.assertEqual(mkmod.manifest_with(base, envelopes=()), base)
        m, _ = cbor_decode(mkmod.manifest_with(base, envelopes=[1, 77]))
        self.assertEqual(m[12], [1, 77])
        for bad in ([0], [-1], [1, 1], [1, 2, 3, 4], [0x80000000], [True]):
            with self.assertRaises(SystemExit):
                mkmod.manifest_with(base, envelopes=bad)

    def test_the_identity_of_a_generated_manifest(self):
        name, tag = mkmod.manifest_identity(bytes.fromhex(Manifest.DEMO))
        m, _ = cbor_decode(bytes.fromhex(Manifest.DEMO))
        self.assertEqual((name, tag), (m[1], m[2]))

    def test_chip_windows_match_the_loader(self):
        loader = read("components", "xip_loader", "src", "loader.c")
        for chip, ifdef in (("esp32s3", "#if CONFIG_IDF_TARGET_ESP32S3\n#define XIP_FIXED"),
                            ("esp32", "#elif CONFIG_IDF_TARGET_ESP32\n#define XIP_FIXED"),
                            ("esp32c6", "#elif CONFIG_IDF_TARGET_ESP32C6\n#define XIP_FIXED")):
            block = loader[loader.index(ifdef):]
            code = int(re.search(r"XIP_CODE_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
            data = int(re.search(r"XIP_DATA_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
            self.assertEqual((code, data), (mkmod.CHIPS[chip]["code_base"], mkmod.CHIPS[chip]["data_base"]), chip)

    def test_slot_spans_reserve_consecutive_windows(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "SLOTS")
            with open(path, "w") as f:
                f.write("nan 7 2\nother 9\n")
            self.assertEqual(mkmod.read_slot_layout(path), {"nan": (7, 2), "other": (9, 1)})
            for bad in ("nan 7 2\nother 8\n", "nan 15 2\n", "nan 7 0\n"):
                with open(path, "w") as f:
                    f.write(bad)
                with self.assertRaises(ValueError):
                    mkmod.read_slots(path)

    def test_the_window_slot_matches_the_loader_check(self):
        check = read("components", "xip_loader", "include", "xip_check.h")
        self.assertEqual(int(re.search(r"XIP_DMOD_WINDOW_SLOT_BYTES (0x[0-9a-fA-F]+)u", check).group(1), 16), mkmod.WINDOW_SLOT)
        self.assertEqual(mkmod.MAX_IMAGE, mkmod.WINDOW_SLOT)


class Packages(unittest.TestCase):
    """The package format of the upload port (docs/development.md, "Formats"): key (65) | signature (64) | header (216) | block table | image.
    The committed vectors were made once by the frozen loader tools; mod_flash's own tests verify and install them."""
    DATA = os.path.join(ROOT, "modules", "mod_flash", "tests", "data")

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
        key, sig, _, name, target, version, image = self.parts(Path(self.DATA, "module-lora-esp32c6.pkg").read_bytes())
        self.assertEqual((key[0], name, target), (4, b"lora", 4))
        self.assertNotEqual(sig, bytes(64))
        _, _, _, _, target, version, image = self.parts(Path(self.DATA, "loader-esp32c6.pkg").read_bytes())
        self.assertEqual((target, version, image[0]), (2, 9, 0xe9))



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
    DEMO = Path(ROOT, "tests", "data", "manifest-demo.hex").read_text().strip()

    def test_the_manifest_is_the_one_ssh_mesh_writes(self):
        """mkmod adds what only the build knows to the manifest mesh-api-gen wrote (crates/mesh-api-gen/tests/data there)."""
        data = os.path.join(ROOT, "tests", "data")
        base = bytes.fromhex(self.DEMO)
        self.assertEqual(mkmod.manifest_with(base), base)
        self.assertEqual(mkmod.manifest_with(base, autostart="").hex(),
                         Path(data, "manifest-demo-autostart.hex").read_text().strip())
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
        m, _ = cbor_decode(dmod.manifest_of(Path(image_path).read_bytes()))
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


class GenVtables(unittest.TestCase):
    """tools/gen_vtables.py: the table and the glue of a hosted component, from its API.md and the C types."""

    def run_tool(self, api, types, directory):
        abi, mod = os.path.join(directory, "abi.rs"), os.path.join(directory, "mod.rs")
        subprocess.run([sys.executable, os.path.join(ROOT, "tools/gen_vtables.py"), "--api", api, "--types", types,
                        "--abi-out", abi, "--module-out", mod], check=True, capture_output=True, cwd=ROOT)
        return Path(abi).read_text(), Path(mod).read_text()

    def test_the_checked_in_files_are_what_the_tool_writes(self):
        with tempfile.TemporaryDirectory() as d:
            abi, mod = self.run_tool("modules/mod_ble/API.md", "crates/xip-abi/src/ble.rs", d)
        self.assertEqual(abi, read("crates", "xip-abi", "src", "ble_vtable.rs"))
        self.assertEqual(mod, read("modules", "mod_ble", "src", "api_gen.rs"))

    def test_power_dispatch_preserves_native_abi_and_excludes_trace_tables(self):
        with tempfile.TemporaryDirectory() as d:
            mod = os.path.join(d, "power.rs")
            subprocess.run([sys.executable, os.path.join(ROOT, "tools/gen_vtables.py"),
                            "--api", "modules/mod_nan/API.md", "--types", "crates/xip-abi/src/power.rs",
                            "--dispatch-only", "--methods", "powerinfo,powersleep,dump_locks", "--module-out", mod], check=True, capture_output=True, cwd=ROOT)
            glue = Path(mod).read_text()
        self.assertEqual(glue, read("modules", "mod_nan", "src", "power_gen.rs"))
        self.assertIn("w.map(27)", glue)
        self.assertNotIn("r.phase", glue)
        self.assertNotIn("pub struct Hosted", glue)
        self.assertNotIn("pub struct Power {", glue)
        self.assertIn("M_DUMP_LOCKS", glue)
        self.assertNotIn("M_DUMP_TASKS", glue)

    def test_one_function_per_method_in_catalog_order(self):
        abi = read("crates", "xip-abi", "src", "ble_vtable.rs")
        names = re.findall(r"^    pub (\w+): Option<", abi, re.M)
        self.assertEqual(names, ["start", "stop", "scan", "status", "scan_stop", "connect", "listen", "bonds_clear", "mode", "scan_results", "suspend", "resume"])
        api = read("modules", "mod_ble", "API.md")
        self.assertEqual([int(n) for n in re.findall(r"^## (\d+)\. `", api, re.M)], [80, 81, 82, 83, 84, 85, 87, 88, 89, 90, 91, 92])
        glue = read("modules", "mod_ble", "src", "api_gen.rs")
        for number, name in zip([80, 81, 82, 83, 84, 85, 87, 88, 89, 90, 91, 92], names):
            self.assertIn(f"pub const M_{name.upper()}: i32 = {number};", glue)

    def test_local_only_methods_are_in_the_table_but_not_reachable_by_envelope(self):
        with tempfile.TemporaryDirectory() as d:
            api = os.path.join(d, "API.md")
            shutil.copy(os.path.join(ROOT, "modules/mod_ble/API.md"), api)
            Path(d, "API.local.md").write_text("## `peek` \u2014 Read a word\n\nLocal use only.\n\n**C function:** `(addr: u32, out: *mut u8)`\n")
            abi, mod = self.run_tool(api, os.path.join(ROOT, "crates/xip-abi/src/ble.rs"), d)
        self.assertIn("pub peek: Option<unsafe extern \"C\" fn(ctx: *mut c_void, addr: u32, out: *mut u8) -> c_int>,", abi)
        self.assertLess(abi.index("pub resume:"), abi.index("pub peek:"), "local methods follow the remote ones")
        self.assertIn("fn peek(&mut self, hw: &Hw, addr: u32, out: *mut u8) -> Result<(), i32>;", mod)
        self.assertNotIn("M_PEEK", mod)
        self.assertIn("M_RESUME", mod)

    def test_a_field_without_a_matching_struct_field_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            types = os.path.join(d, "ble.rs")
            Path(types).write_text(read("crates", "xip-abi", "src", "ble.rs").replace("pub paired: u8,", "pub renamed: u8,"))
            r = subprocess.run([sys.executable, os.path.join(ROOT, "tools/gen_vtables.py"), "--api", os.path.join(ROOT, "modules/mod_ble/API.md"),
                                "--types", types, "--abi-out", os.path.join(d, "a.rs"), "--module-out", os.path.join(d, "m.rs")], capture_output=True, text=True)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("paired", r.stderr)


if __name__ == "__main__":
    unittest.main()
