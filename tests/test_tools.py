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
import gen_abi  # noqa: E402
import mkmod  # noqa: E402
import loader_tools  # noqa: E402,F401  (esp-wifi-loader's tools)
import sign  # noqa: E402
import wl_format as wf  # noqa: E402


def read(*path):
    with open(os.path.join(ROOT, *path)) as f:
        return f.read()


class Abi(unittest.TestCase):
    def test_generated_files_are_current(self):
        r = subprocess.run([sys.executable, os.path.join(ROOT, "tools", "gen_abi.py"), "--check"], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_lock_detects_a_changed_slot(self):
        d = gen_abi.load()
        lock = gen_abi.lock_data(d)
        self.assertEqual(gen_abi.lock_problems(d, lock), [])
        lock["entries"][3] = "int sleep_ms(int ms)"
        self.assertTrue(any("slot 3 changed" in p for p in gen_abi.lock_problems(d, lock)))
        lock = gen_abi.lock_data(d)
        lock["entries"].append("int removed(void)")
        self.assertTrue(any("removed" in p for p in gen_abi.lock_problems(d, lock)))
        lock = gen_abi.lock_data(d)
        lock["entries"] = lock["entries"][:-2]          # an older release: appended slots are fine
        self.assertEqual(gen_abi.lock_problems(d, lock), [])

    def test_header_matches_the_host_table_size(self):
        n = len(gen_abi.load()["entry"])
        self.assertIn(f"== {16 + 4 * n}, \"espmod_host layout\"", read("include", "espmod.h"))

    def test_python_and_c_agree_on_the_image_header(self):
        header = read("include", "espmod.h")
        self.assertIn(f"#define ESPMOD_DMOD_MAGIC 0x{dmod.MAGIC:x}u", header)
        self.assertIn(f"#define ESPMOD_ABI_VERSION {dmod.ABI_VERSION}u", header)
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
    def test_shared_product_slots_preserve_the_installed_layout(self):
        s = mkmod.slots()
        self.assertEqual(s["console"], (20, 0x50000, 0x10000))
        self.assertEqual(s["test"], (2, 0x40000, 0x10000))
        self.assertEqual(s, mkmod.slots(os.path.join(ROOT, "main", "main.c")))
        tags = [t for t, _, _ in s.values()]
        self.assertEqual(len(set(tags)), len(tags))
        spans = sorted((o, o + z) for _, o, z in s.values())
        for (a0, a1), (b0, b1) in zip(spans, spans[1:]):
            self.assertLessEqual(a1, b0)
        for _, o, z in s.values():
            self.assertEqual((o % 0x10000, z % 0x10000), (0, 0))

    def test_chip_windows_match_the_loader(self):
        loader = read("components", "espmod_loader", "src", "loader.c")
        for chip, ifdef in (("esp32s3", "#if CONFIG_IDF_TARGET_ESP32S3\n#define EM_FIXED"),
                            ("esp32", "#elif CONFIG_IDF_TARGET_ESP32\n#define EM_FIXED")):
            block = loader[loader.index(ifdef):]
            code = int(re.search(r"EM_CODE_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
            data = int(re.search(r"EM_DATA_BASE (0x[0-9a-fA-F]+)u", block).group(1), 16)
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


class FakeDevice:
    """Just enough of a main for tests/device_test.py: the control commands of docs/protocol.md and the checks of
    the upload service (key, version, chip)."""

    def __init__(self, trust_pub, cpu):
        self.trust, self.cpu = trust_pub, cpu
        self.version, self.installed, self.running, self.runs, self.mode = 0, False, False, 0, ""
        self.events = ""
        self.sent = []

    def control(self, line, timeout=15):
        cmd, _, rest = line.partition(" ")
        if cmd == "status":
            return (f"name=hello tag=1 installed=false running=false version=0 last=0 runs=0 ms=0 stack_free=0 check=0\n"
                    f"name=test tag=2 installed={str(self.installed).lower()} running={str(self.running).lower()} "
                    f"version={self.version} last=0 runs={self.runs} ms=3 stack_free=2000 check=0\n")
        if cmd == "wifi":
            return {"start": "state=joining\n", "stop": "state=off\n"}.get(rest, "state=up ssid=\"DIRECT-ef-flash\" ip=192.168.49.9 connected=1 rssi=-40 saved_ssid=\"(flash network)\" psk=unset\n")
        if cmd == "loader":
            return "ok\n"
        if cmd == "events":
            return self.events
        if cmd == "start" and self.installed:
            self.running = rest.startswith("test serve")
            if rest.startswith("test alarm"):
                self.events += "module=test id=32511 pass=3 fail=0\nmodule=test id=32270 oneshot ok\nmodule=test id=32271 periodic ok\nmodule=test id=32272 periodic-cancel ok\n"
            self.mode = rest
            if not self.running:
                self.runs += 1
                self.events += "module=test id=32511 pass=30 fail=0\n"
            return "ok\n"
        if cmd == "test.echo":
            self.events += "module=test id=32528 " + rest + "\n"
            return f"value={rest}\n"
        if cmd == "stop":
            self.running = False
            return "ok\n"
        return "error: unknown method\n"

    def upload(self, body, name=None):
        pub, _, hdr, _, image = wf.split_package(body[16:])
        h = wf.unpack_header(hdr)
        if pub != self.trust:
            return "ERR 8 key-mismatch"
        if h["version"] < self.version:
            return "ERR 9 rollback"
        if h["cpu"] != self.cpu:
            return "ERR 10 wrong-chip-or-target"
        self.version, self.installed = h["version"], True
        return "OK %d" % len(image)


class LabConfig(unittest.TestCase):
    """tests/device.toml is read by esp-wifi-loader's provision.py and by device_test.py: every board is complete."""

    def test_the_lab_config_is_complete(self):
        import tomllib
        cfg = tomllib.load(open(os.path.join(ROOT, "tests", "device.toml"), "rb"))
        for b in cfg["board"]:
            for key in ("name", "chip", "mac", "port", "phone", "kind", "host", "key"):
                self.assertIn(key, b, b["name"])
            self.assertIn(b["chip"], wf.CHIPS)
            self.assertRegex(b["mac"], r"^([0-9a-f]{2}:){5}[0-9a-f]{2}$")


class DeviceScript(unittest.TestCase):
    def test_the_bench_procedure_runs_against_a_device_that_behaves(self):
        import device_test
        d = tempfile.mkdtemp()
        key = os.path.join(d, "k.pem")
        sign.keygen(key)
        mods = os.path.join(d, "esp32c6", "modules")
        os.makedirs(mods)
        open(os.path.join(mods, "mod_test.dmod"), "wb").write(dmod.pack(b"\0" * 4000, 0x44, 2))
        device = FakeDevice(sign.public_key(key), wf.CHIPS["esp32c6"])
        board = dict(name="fake", chip="esp32c6", host="x", key=key)
        device_test.run_board(board, d, t=device, sleep=lambda s: None)
        self.assertFalse(device.running)         # the procedure ends with `stop`
        # a device that accepts a wrong key must fail the procedure
        bad = FakeDevice(sign.public_key(key), wf.CHIPS["esp32c6"])
        bad.upload = lambda body, name=None: "OK 1"
        with self.assertRaises(device_test.Failed):
            device_test.run_board(board, d, t=bad, sleep=lambda s: None)


@unittest.skipUnless(shutil.which("xtensa-esp32-elf-gcc") or shutil.which("riscv32-esp-elf-gcc"), "needs the ESP toolchains (. ./env.sh)")
class Build(unittest.TestCase):
    """The modules really build, and what comes out is what the loader accepts."""

    def build(self, chip, slot, src):
        tool = mkmod.CHIPS[chip]["prefix"] + "-gcc"
        if not shutil.which(tool):
            self.skipTest(tool)
        out = os.path.join(tempfile.mkdtemp(), "m.dmod")
        return mkmod.build(chip, slot, [os.path.join(ROOT, "modules", src)], out)

    def test_all_chips(self):
        for chip in mkmod.CHIPS:
            for slot, src in (("hw", "mod_hello/mod_hello.c"), ("test", "mod_test/mod_test.c")):
                with self.subTest(chip=chip, slot=slot):
                    image = self.build(chip, slot, src)
                    h = dmod.unpack(image)
                    cfg = mkmod.CHIPS[chip]
                    tag, offset, size = mkmod.slots()[slot]
                    self.assertEqual(h["tag"], tag)
                    self.assertLessEqual(len(image), size)
                    self.assertEqual(h["flags"] & 1, 1 if cfg["fixed"] else 0)
                    self.assertEqual(h["code_vma"], cfg["code_base"] + offset if cfg["fixed"] else 0)

    def test_the_package_passes_the_device_checks(self):
        exe = os.path.join(tempfile.mkdtemp(), "test_check")
        subprocess.run(["gcc", "-Wall", "-Wextra", "-fsanitize=address,undefined", "-I", "include", "-I", "components/espmod_loader/include",
                        "-I", "tests/host", "-o", exe, "tests/host/test_check.c",
                        "components/espmod_loader/src/espmod_check.c"], cwd=ROOT, check=True)
        key = os.path.join(tempfile.mkdtemp(), "k.pem")
        sign.keygen(key)
        for chip, cpu in (("esp32c6", 13), ("esp32s3", 9), ("esp32", 0)):
            if not shutil.which(mkmod.CHIPS[chip]["prefix"] + "-gcc"):
                continue
            image = self.build(chip, "test", "mod_test/mod_test.c")
            path = os.path.join(tempfile.mkdtemp(), "t.wlpkg")
            open(path, "wb").write(sign.make_package(key, chip, 1, "test", image, "module"))
            r = subprocess.run([exe, path, "test", str(cpu), hex(mkmod.slots()["test"][1])], capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, chip + "\n" + r.stdout)

    def test_a_module_with_state_is_rejected(self):
        src = os.path.join(tempfile.mkdtemp(), "bad.c")
        open(src, "w").write('#include "espmod_module.h"\nstatic volatile int counter;\n'
                             'ESPMOD_HANDLER(bad)(const espmod_host *h, void *s, const espmod_call *c) { (void)h; (void)s; (void)c; return ++counter; }\n')
        chip = "esp32c6" if shutil.which("riscv32-esp-elf-gcc") else "esp32"
        with self.assertRaises(SystemExit) as cm:
            mkmod.build(chip, "lora", [src], os.path.join(tempfile.mkdtemp(), "x.dmod"))
        self.assertIn(".bss", str(cm.exception))

    def test_a_module_needing_libc_is_rejected(self):
        src = os.path.join(tempfile.mkdtemp(), "bad.c")
        open(src, "w").write('#include "espmod_module.h"\nextern int puts(const char *);\n'
                             'ESPMOD_HANDLER(bad)(const espmod_host *h, void *s, const espmod_call *c) { (void)h; (void)s; (void)c; return puts("x"); }\n')
        chip = "esp32c6" if shutil.which("riscv32-esp-elf-gcc") else "esp32"
        with self.assertRaises(SystemExit) as cm:
            mkmod.build(chip, "lora", [src], os.path.join(tempfile.mkdtemp(), "x.dmod"))
        self.assertIn("puts", str(cm.exception))


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
    API = """# `demo` API (7)

## 1. `ping` — x

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |
| 2 | `wait` | `uint` | |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |

## 20. `pong` — y

### Request

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |
| 2 | `wait` | `uint` | |

### Response

| Tag | Field | Type | Description |
|---:|---|---|---|
| 1 | `value` | `string` | |
"""

    def test_the_manifest_is_the_catalog_of_the_api_md(self):
        m, end = cbor_decode(mkmod.manifest_of_api(self.API))
        self.assertEqual(m[1], "demo")
        self.assertEqual(m[2], 7)
        self.assertEqual([(x[1], x[2]) for x in m[3]], [(1, "ping"), (20, "pong")])
        requests = m[5][m[3][0][3]]
        self.assertEqual(requests, [[1, "value", 1], [2, "wait", 3]])        # tag, name, kind (text, uint)
        self.assertEqual(m[3][0][3], m[3][1][3], "an identical field list is stored once")
        self.assertEqual(len(m[5]), 2)
        self.assertEqual(mkmod.manifest_of_api("no module here"), b"")

    def test_the_manifest_is_the_one_ssh_mesh_writes(self):
        """The same API.md and the same bytes as ssh-mesh's mesh-api-gen (crates/mesh-api-gen/tests/data, a test there)."""
        data = os.path.join(ROOT, "tests", "data")
        api = open(os.path.join(data, "manifest-demo-API.md")).read()
        self.assertEqual(mkmod.manifest_of_api(api).hex(), open(os.path.join(data, "manifest-demo.hex")).read().strip())
        self.assertEqual(mkmod.manifest_of_api(api, autostart="").hex(),
                         open(os.path.join(data, "manifest-demo-autostart.hex")).read().strip())
        m, _ = cbor_decode(mkmod.manifest_of_api(api, autostart="serve"))
        self.assertEqual(m[6], "serve")
        self.assertEqual(m[4][0][0], "Row")

    def test_flash_slot_declaration_is_in_manifest(self):
        m, _ = cbor_decode(mkmod.manifest_of_api(self.API, flash_slots=(3, 1, 3)))
        self.assertEqual(m[7], [1, 3])
        self.assertNotIn(7, cbor_decode(mkmod.manifest_of_api(self.API))[0])

    def test_the_manifest_sits_after_the_code_inside_the_image(self):
        m = mkmod.manifest_of_api(self.API)
        image = dmod.pack(b"\x01\x02\x03\x04" * 5, 0x44, 7, manifest=m)
        h = dmod.unpack(image)
        self.assertEqual(h["manifest_offset"], 64 + 20)
        self.assertEqual(h["manifest_size"], len(m))
        self.assertEqual(h["image_size"], len(image))
        self.assertEqual(dmod.manifest_of(image), m)
        self.assertEqual(len(image) % 4, 0)
        plain = dmod.unpack(dmod.pack(b"\x01\x02\x03\x04" * 2, 0x44, 7))
        self.assertEqual((plain["manifest_offset"], plain["manifest_size"]), (0, 0))

    def test_a_built_module_carries_its_manifest(self):
        image_path = os.path.join(ROOT, "target", "out", "esp32c6", "modules", "mod_test.dmod")
        if not os.path.exists(image_path):
            self.skipTest("no built image (scripts/build.sh)")
        m, _ = cbor_decode(dmod.manifest_of(open(image_path, "rb").read()))
        self.assertEqual((m[1], m[2]), ("test", 2))
        self.assertIn("echo", [x[2] for x in m[3]])


class SlotTable(unittest.TestCase):
    def test_a_rust_slot_table_is_read_like_the_c_one(self):
        path = os.path.join(tempfile.mkdtemp(), "slots.rs")
        with open(path, "w") as f:
            f.write('static S: [Slot; 2] = [slot!("lora", 43, 0x00000, 0x10000), slot!("hw", 45, 0x10000, 0x10000)];\n')
        self.assertEqual(mkmod.slots(path), {"lora": (43, 0, 0x10000), "hw": (45, 0x10000, 0x10000)})
        self.assertIn("test", mkmod.slots())      # shared product table


class UpdateNetwork(unittest.TestCase):
    """The bench delegates uploads to its configured loader endpoint."""
    def test_upload_uses_loader_without_main_installer_commands(self):
        import device_test
        t = device_test.Transport("192.0.2.1", None, "unused")
        t.control = lambda *args: self.fail("upload must not start firmware services")
        original = device_test.upload.send
        sent = []
        device_test.upload.send = lambda host, port, body: sent.append((host, port, body)) or "OK"
        try:
            self.assertEqual(t.upload(b"h" * 16 + b"image", "test"), "OK")
        finally:
            device_test.upload.send = original
        self.assertEqual(sent, [("192.0.2.1", 5227, b"image")])


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
        c.ser = Ser([b"W (1234) espmod: test: log line\r\n", b"name=test tag=2 installed=true\r\n",
                     b"W (1240) main: test event 0x7eff: pass=1\r\n"])
        self.assertEqual(c.command("status", timeout=3, idle=0.01), "name=test tag=2 installed=true\n")
        self.assertEqual(c.ser.sent, b"status\n")
        c.ser = Ser([])
        self.assertEqual(c.command("status", timeout=0.2, idle=0.01), "no reply")


if __name__ == "__main__":
    unittest.main()
