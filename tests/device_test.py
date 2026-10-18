#!/usr/bin/env python3
"""The bench procedure of docs/testing.md as a script: runs mod_rtest on real boards.

  tests/device_test.py [--config tests/device.toml] [--board NAME] [--out target/out]

Needs the images from scripts/build.sh. Boards come from the config file only (tests/device.toml: the current lab). Exit status 0 = every board passed.
"""
import argparse
import os
import re
import sys
import tempfile
import time
import tomllib

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
sys.path.insert(0, os.path.join(ROOT, "tools"))
import loader_tools  # noqa: E402,F401  (esp-wifi-loader's tools)
import console  # noqa: E402
import sign  # noqa: E402
import upload  # noqa: E402


MODULE_PORT = 5227      # the upload port of main (esp-wifi-loader's): upload.py's default


class Transport:
    """What the test needs from a board; tests/test_tools.py replaces it with a fake device. Commands go over the
    serial console, packages over the network (upload port)."""

    def __init__(self, host, phone=None, port=None):
        self.host, self.phone, self.port = host, phone, port    # phone: carry the bytes through its netcat (no Wi-Fi on this computer)
        self._console = None
        self.sleep = time.sleep

    def control(self, line, timeout=15):
        if self._console is None:
            self._console = console.Console(self.port).open()
        return self._console.command(line, timeout=timeout)

    def close(self):
        if self._console:
            self._console.close()
            self._console = None

    def upload(self, body: bytes, name):
        """Send to the loader endpoint selected by the bench control plane.

        This runtime starts no built-in installer. The loader performs writes;
        its provisioning/reset/startup is owned by the selected bench setup.
        """
        if not self.host:
            raise Failed("a loader upload endpoint is required")
        if self.phone:
            return upload.send_adb(self.phone, self.host, MODULE_PORT, body[16:])
        return upload.send(self.host, MODULE_PORT, body[16:])


class Failed(Exception):
    pass


def check(cond, what):
    print(("  ok    " if cond else "  FAIL  ") + what, flush=True)
    if not cond:
        raise Failed(what)


def slot_line(t, name="test"):
    return [l for l in t.control("status").splitlines() if l.startswith(f"name={name} ")][0]


def last_summary(text):
    """The final summary event (0x7eff = 32511) of the latest mod_rtest run in the console's `events` ring."""
    lines = [l for l in text.splitlines() if " id=32511 " in l]
    return lines[-1] if lines else ""


def events_since(t, mark):
    return t.control("events")[mark:]


def run_board(board, out, t=None, sleep=time.sleep):
    own = t is None
    t = t or Transport(board["host"], board.get("phone") if board.get("via") == "adb" else None, board.get("port"))
    try:
        _run_board(board, out, t, sleep)
    finally:
        if own:
            t.close()


def _run_board(board, out, t, sleep):
    chip, key = board["chip"], os.path.expanduser(board["key"])
    image = os.path.join(out, chip, "modules", "mod_rtest.dmod")
    version = int(time.strftime("%y%m%d%H%M", time.gmtime()))   # the scheme of scripts/publish-release.sh
    pkg = sign.make_package(key, chip, version, "test", open(image, "rb").read(), "module")
    from wl_format import transport_header
    body = transport_header() + pkg

    print(f"[{board['name']}] {chip} at {board['host']}")
    check("name=test " in t.control("status"), "console answers, slot 'test' exists")

    status = t.upload(body, "test")
    check(status.startswith("OK"), f"signed upload accepted ({status})")
    check(re.search(r"name=test tag=2 installed=true running=false version=%d" % version, t.control("status")) is not None,
          "status shows the installed version")

    runs_before = int(re.search(r"runs=(\d+)", slot_line(t)).group(1))     # the board keeps counting between test runs
    check(t.control("start test").startswith("ok"), "start test")
    sleep(3)
    status_line = slot_line(t)
    check("last=0" in status_line and f"runs={runs_before + 1} " in status_line, f"flashed mod_rtest passed ({status_line})")
    check("fail=0" in last_summary(t.control("events")), "flashed run reports fail=0")

    check(t.control("start test alarm").startswith("ok"), "start test alarm")
    sleep(2)
    events = t.control("events")     # the ring is short: the alarm run follows the 40 events of the self test
    check("oneshot ok" in events and "periodic ok" in events and "periodic-cancel ok" in events and "FAIL" not in events,
          "one-shot and periodic alarms fire on time")

    check(t.control("start test serve").startswith("ok"), "start test serve")
    sleep(1)
    check(t.control("test.echo hello").startswith("value=hello"), "a request is answered")
    sleep(1)
    check("hello" in t.control("events"), "the module echoed the request as an event")

    new_version = version + 1
    pkg2 = sign.make_package(key, chip, new_version, "test", open(image, "rb").read(), "module")
    check(t.upload(transport_header() + pkg2, "test").startswith("OK"), "upload while running (hot replace)")
    sleep(2)
    status_line = [l for l in t.control("status").splitlines() if l.startswith("name=test ")][0]
    check("running=true" in status_line and f"version={new_version}" in status_line, f"restarted by itself ({status_line})")
    check(t.control("test.echo again").startswith("value=again"), "the replaced module accepts requests")

    other = os.path.join(tempfile.mkdtemp(), "other.pem")
    sign.keygen(other)
    wrong = t.upload(transport_header() + sign.make_package(other, chip, new_version + 1, "test", open(image, "rb").read(), "module"), "test")
    check(wrong.startswith("ERR 8"), f"another key is refused ({wrong})")
    old = t.upload(transport_header() + sign.make_package(key, chip, version - 1, "test", open(image, "rb").read(), "module"), "test")
    check(old.startswith("ERR 9"), f"an older version is refused ({old})")
    status_line = [l for l in t.control("status").splitlines() if l.startswith("name=test ")][0]
    check("running=true" in status_line, "the running module was not disturbed")

    check(t.control("stop test").startswith("ok"), "stop returns")

    if "gpio" in board:
        a, b = board["gpio"]
        mark = len(t.control("events"))
        t.control(f"start test gpio {a} {b}")
        sleep(2)
        check("FAIL" not in events_since(t, mark) and "irq-event ok" in events_since(t, mark), f"gpio loop {a}-{b} and its interrupt")
    if "spi" in board:
        mark = len(t.control("events"))
        t.control("start test spi " + " ".join(str(p) for p in board["spi"]))
        sleep(2)
        check("FAIL" not in events_since(t, mark) and "loopback ok" in events_since(t, mark), "spi loopback")
    print(f"[{board['name']}] passed")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--config", default=os.path.join(ROOT, "tests", "device.toml"))
    ap.add_argument("--board")
    ap.add_argument("--out", default=os.path.join(ROOT, "target", "out"))
    a = ap.parse_args()
    if not os.path.exists(a.config):
        sys.exit(f"no {a.config}: describe your boards (see provision.py and docs/flashing.md)")
    cfg = tomllib.load(open(a.config, "rb"))
    if cfg.get("adb"):
        os.environ.setdefault("ADB", os.path.expanduser(cfg["adb"]))
        upload.ADB = os.environ["ADB"]
    boards = cfg["board"]
    failed = 0
    for board in boards:
        if a.board and board["name"] != a.board:
            continue
        try:
            run_board(board, a.out)
        except Failed as e:
            print(f"[{board['name']}] FAILED: {e}")
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
