"""The module sink of esp-wifi-loader, components/espmod_loader/src/install.c, compiled for the host against a RAM
flash (tests/host/fake_loader.c) and driven in the order the loader drives it, with packages made by sign.py. What
the loader verifies (key, signature, block hashes, reboot requests, garbage) is tested in esp-wifi-loader."""
import os
import random
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
LOADER = os.environ.get("ESP_WIFI_LOADER_DIR") or os.path.join(ROOT, "..", "esp-wifi-loader")
sys.path.insert(0, os.path.join(ROOT, "tools"))
import dmod  # noqa: E402
import loader_tools  # noqa: E402,F401  (esp-wifi-loader's tools)
import sign  # noqa: E402
import wl_format as wf  # noqa: E402

SOURCES = ["tests/host/test_install.c", "tests/host/fake_loader.c", "components/espmod_install/src/install.c",
           "components/espmod_loader/src/slots.c", "components/espmod_loader/src/espmod_check.c"]


def module_image(tag, size=6000, seed=1):
    rnd = random.Random(seed)
    return dmod.pack(bytes(rnd.randrange(256) for _ in range(size)), 0x44, tag)


@unittest.skipUnless(shutil.which("gcc"), "needs gcc")
class Install(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.dir = tempfile.mkdtemp()
        cls.exe = os.path.join(cls.dir, "test_install")
        subprocess.run(["gcc", "-Wall", "-Wextra", "-g", "-fsanitize=address,undefined", "-DCONFIG_IDF_FIRMWARE_CHIP_ID=13",
                        "-I", "tests/host/fake_idf", "-I", "tests/host", "-I", "include", "-I", "components/espmod_loader/include",
                        "-I", "components/espmod_host/include", "-I", "components/espmod_pool/include", "-I", "components/espmod_loader/src", "-I", "components/espmod_install/include",
                        "-I", os.path.join(LOADER, "components", "esp_wifi_loader", "include"), "-o", cls.exe,
                        *SOURCES, "-lpthread"], cwd=ROOT, check=True)
        cls.key = os.path.join(cls.dir, "a.pem")
        sign.keygen(cls.key)
        cls.n = 0

    def body(self, image, name="test", version=1):
        pkg = sign.make_package(self.key, "esp32c6", version, name, image, "module")
        type(self).n += 1
        path = os.path.join(self.dir, f"body{self.n}.bin")
        with open(path, "wb") as f:
            f.write(wf.transport_header() + pkg)
        return path

    def run_script(self, *lines):
        script = os.path.join(self.dir, "script.txt")
        with open(script, "w") as f:
            f.write("reset\n" + "\n".join(lines) + "\nexpect violations 0\n")
        r = subprocess.run([self.exe, script], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    # ---- the good paths ----
    def test_install_and_reinstall(self):
        img = module_image(2)
        p1, p2 = self.body(img, version=3), self.body(module_image(2, seed=2), version=4)
        self.run_script(f"send {p1} OK", f"expect image 1 {p1}", "expect version 1 3", "expect starts 0",
                        f"send {p1} OK",                       # the same version again is fine
                        f"send {p2} OK", f"expect image 1 {p2}", "expect version 1 4")

    def test_small_and_block_sized_images(self):
        lines = []
        for i, size in enumerate((70, 4096 - 64, 4096 - 63, 4096 + 100, 8192 - 64)):
            p = self.body(module_image(2, size=size, seed=i), version=i + 1)
            lines += [f"send {p} OK", f"expect image 1 {p}"]
        self.run_script(*lines)

    @unittest.skipUnless(shutil.which("riscv32-esp-elf-gcc"), "needs the ESP toolchains (. ./env.sh)")
    def test_a_module_built_by_mkmod(self):
        import mkmod
        src = os.path.join(ROOT, "modules", "mod_test", "mod_test.c")
        image = mkmod.build("esp32c6", "test", [src], os.path.join(self.dir, "mod_test.dmod"))
        p = self.body(image, name="test", version=11)
        self.run_script(f"send {p} OK", f"expect image 1 {p}", "expect version 1 11")

    def test_every_slot_by_name(self):
        lines = []
        for slot, (name, tag) in enumerate([("hello", 1), ("test", 2), ("lora", 43), ("hw", 45)]):
            p = self.body(module_image(tag), name=name)
            lines += [f"send {p} OK", f"expect image {slot} {p}"]
        self.run_script(*lines)

    def test_dry_run_changes_nothing(self):
        p = self.body(module_image(2))
        self.run_script(f"dryrun {p} OK", "expect untouched", "expect version 1 0", "expect stops 0")

    def test_hot_replace_stops_installs_restarts(self):
        p = self.body(module_image(2), version=5)
        self.run_script("running 1", f"send {p} OK", "expect stops 1", "expect starts 1", f"expect image 1 {p}")

    def test_not_running_module_is_not_started(self):
        p = self.body(module_image(2))
        self.run_script(f"send {p} OK", "expect starts 0")

    # ---- everything that must be refused before the flash is touched ----
    def test_rollback(self):
        new, old = self.body(module_image(2), version=7), self.body(module_image(2, seed=3), version=6)
        self.run_script(f"send {new} OK", f"send {old} ERR 9", "expect untouched", "expect version 1 7")



    def test_unknown_module(self):
        p = self.body(module_image(2), name="nosuch")
        self.run_script(f"send {p} ERR 15", "expect untouched")



    def test_image_for_another_slot(self):
        p = self.body(module_image(3), name="test")        # tag 3 is not a slot of this table (43 is lora)
        self.run_script(f"send {p} ERR 4", "expect untouched")

    def test_image_larger_than_the_slot(self):
        p = self.body(module_image(2, size=70000))
        self.run_script(f"send {p} ERR 3", "expect untouched")

    def test_module_that_is_not_a_module(self):
        p = self.body(bytes(8192))
        self.run_script(f"send {p} ERR 4", "expect untouched")

    def test_module_that_does_not_stop(self):
        p = self.body(module_image(2))
        self.run_script("running 1", "stopfail -107", f"send {p} ERR 16", "expect untouched")



    # ---- failures after the erase: the slot must end up without a valid header ----
    def test_interrupted_install_leaves_no_valid_module(self):
        p = self.body(module_image(2, size=20000), version=3)
        self.run_script(f"interrupt {p} 2", "expect empty 1", "expect version 1 0", "expect starts 0")

    def test_interrupted_hot_replace_does_not_restart_the_erased_slot(self):
        p = self.body(module_image(2, size=20000), version=3)
        self.run_script("running 1", f"interrupt {p} 1", "expect empty 1", "expect stops 1", "expect starts 0")


if __name__ == "__main__":
    unittest.main()
