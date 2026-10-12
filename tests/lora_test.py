#!/usr/bin/env python3
"""Two LoRa boards exchange packets (docs/testing.md): the lora module on A and B, in both directions, with a
transmission in the middle of a reception, and in CAD mode.

  tests/lora_test.py --a lora1 --b lora3        boards of tests/device.toml (both with a radio, in range, running main)

Needs target/out/<chip>/modules/mod_lora.dmod (scripts/build.sh) and the signing key of the board entries.
"""
import argparse
import os
import sys
import time
import tomllib

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import device_test as dt   # noqa: E402
import sign   # noqa: E402
from wl_format import transport_header   # noqa: E402


class Board:
    def __init__(self, cfg, out):
        self.name, self.cfg = cfg["name"], cfg
        self.t = dt.Transport(cfg["host"], cfg.get("phone") if cfg.get("via") == "adb" else None, cfg.get("port"))
        image = os.path.join(out, cfg["chip"], "modules", "mod_lora.dmod")
        version = int(time.strftime("%y%m%d%H%M", time.gmtime()))
        self.package = transport_header() + sign.make_package(os.path.expanduser(cfg["key"]), cfg["chip"], version, "lora",
                                                              open(image, "rb").read(), "module")

    def control(self, line):
        return self.t.control(line)

    def install(self):
        status = self.t.upload(self.package, "lora")
        dt.check(status.startswith("OK"), f"[{self.name}] lora module installed ({status.strip()})")
        self.control("stop lora")
        dt.check(self.control("start lora idle").startswith("ok"), f"[{self.name}] started idle")

    def events(self):
        return self.control("events")


def hexof(text):
    return text.encode().hex()


def seen(board, text, wait=8, sleep=time.sleep):
    for _ in range(wait):
        if hexof(text) in board.events():
            return True
        sleep(1)
    return False


def run(a, b, sleep=time.sleep):
    for board in (a, b):
        board.install()
    dt.check(a.control("lora.probe").startswith("result=0"), f"[{a.name}] probe accepted")
    sleep(1)
    dt.check("id=31 01000000" in a.events(), f"[{a.name}] radio found (SX127x)")

    dt.check(b.control("lora.rx").startswith("result=0"), f"[{b.name}] rx")
    sleep(1)
    tag = "a2b-%d" % (time.time() % 1000)
    dt.check(a.control(f"lora.tx data=hex:{hexof(tag)}").startswith("result=0"), f"[{a.name}] tx {tag}")
    dt.check(seen(b, tag, sleep=sleep), f"[{b.name}] received {tag}")

    dt.check(a.control("lora.rx").startswith("result=0"), f"[{a.name}] rx")
    sleep(1)
    tag = "b2a-%d" % (time.time() % 1000)
    b.control(f"lora.tx data=hex:{hexof(tag)}")
    dt.check(seen(a, tag, sleep=sleep), f"[{a.name}] received {tag} (b's transmission came in the middle of its reception)")
    tag = "a2b-again-%d" % (time.time() % 1000)
    a.control(f"lora.tx data=hex:{hexof(tag)}")
    dt.check(seen(b, tag, sleep=sleep), f"[{b.name}] still receiving after its own transmission")

    for board in (a, b):
        board.control("lora.stop")
    dt.check(b.control("lora.rx cad=1 cad_rx_ms=1500").startswith("result=0"), f"[{b.name}] rx with CAD windows")
    sleep(1)
    tag = "cad-%d" % (time.time() % 1000)
    a.control(f"lora.tx data=hex:{hexof(tag)}")
    dt.check(seen(b, tag, sleep=sleep), f"[{b.name}] received {tag} through a CAD window")
    for board in (a, b):
        board.control("lora.stop")
    print("lora test passed")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--config", default=os.path.join(ROOT, "tests", "device.toml"))
    ap.add_argument("--a", required=True)
    ap.add_argument("--b", required=True)
    ap.add_argument("--out", default=os.path.join(ROOT, "target", "out"))
    args = ap.parse_args()
    cfg = tomllib.load(open(args.config, "rb"))
    if cfg.get("adb"):
        os.environ.setdefault("ADB", os.path.expanduser(cfg["adb"]))
    boards = {b["name"]: b for b in cfg["board"]}
    run(Board(boards[args.a], args.out), Board(boards[args.b], args.out))


if __name__ == "__main__":
    try:
        main()
    except dt.Failed as e:
        sys.exit(f"FAILED: {e}")
