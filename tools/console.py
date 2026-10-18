#!/usr/bin/env python3
"""The board's serial console, from the computer: find the connected boards, read what modules say, send them commands.

  tools/console.py list                                 find the connected boards and ask each for its `status`
  tools/console.py [--port PATH] monitor [--seconds N]  print the console (Ctrl-C to stop); opening it does not reset the board
  tools/console.py [--port PATH] send "status"          one command line, then print the answer (--wait seconds)

There is no list of lab boards: whatever is connected now is discovered (/dev/serial/by-id, else ttyACM/ttyUSB), by asking the console. A sleepy
device answers late (or drops the first bytes while it wakes), so the probe sends a newline and `status` again for up to --probe seconds.
Without --port the only board that answers is used (or $XIP_PORT); with several, `list` shows them and --port picks. Tests that need a device
borrow whatever fits (chip, modules installed) from discover(); a lock file under target/ for borrow/release comes later, and with secrets the console
over USB is the way to unlock or reset a test device.

Resetting the board, including the five quick resets that start the loader, is esp-wifi-loader's reset.py.

Commands (`help` on the board lists them): status, start, stop, wifi, help, reset, nv, heap, COMPONENT.METHOD key=value.

Opening the port does not reset the board (the USB-Serial/JTAG block resets the chip only when DTR goes low while RTS
is high; this tool leaves the lines alone). What modules log() appears as lines like
  W (1234) xip: test: mod_rtest log
"""
import argparse
import glob
import os
import re
import sys
import time

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")


LOG_LINE = re.compile(r"^\x1b?\[?[0-9;]*m?[IWEDV] \(\d+\) ")      # ESP-IDF log lines: "W (1234) tag: text"
MODULE = re.compile(r'\{1:"([^"]+)",2:(\d+),3:(true|false),4:(true|false)')   # a module record of `status`: name, component, installed, running
MAC = re.compile(r"([0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5})")


class Console:
    """Serial command reader; signing/upload tools are independent."""
    def __init__(self, path):
        self.path, self.ser = path, None

    def open(self):
        self.ser = self._open_serial(self.path)
        return self

    def close(self):
        if self.ser is not None:
            self.ser.close()
            self.ser = None

    def read(self, seconds):
        text = []
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            text.append(self.ser.read(256).decode(errors="replace"))
        return "".join(text)

    @staticmethod
    def _open_serial(path):
        # DTR and RTS low before the port opens: on boards with an auto-reset circuit (CP2102 + EN/IO0) asserting them is a reset
        import serial
        ser = serial.Serial()
        ser.port, ser.baudrate, ser.timeout = path, 115200, 0.2
        ser.dtr = ser.rts = False
        ser.open()
        return ser

    def send(self, line):
        self.ser.write((line + "\n").encode())

    def command(self, line, timeout=15.0, idle=0.4):
        """One command and its answer. The answer is every line that is not an ESP-IDF log line; it is complete when
        some has arrived and the port has been quiet for `idle` seconds (a slow command, `start`, answers late)."""
        self.ser.reset_input_buffer()
        self.send(line)
        text, end, last = "", time.time() + timeout, None
        while time.time() < end:
            chunk = self.ser.read(256).decode(errors="replace")
            now = time.time()
            if chunk:
                text += chunk
                last = now
            elif last is not None and now - last >= idle and self._answer(text):
                break
        return self._answer(text) or "no reply"

    @staticmethod
    def _answer(text):
        lines = [l for l in text.replace("\r", "").split("\n") if l.strip() and not LOG_LINE.match(l)]
        return "".join(l + "\n" for l in lines)


def ports():
    """The serial ports that may be a board, stable names first."""
    found = sorted(glob.glob("/dev/serial/by-id/*"))
    return found or sorted(glob.glob("/dev/ttyACM*") + glob.glob("/dev/ttyUSB*"))


def probe(path, seconds=10.0):
    """Ask the console of the board on `path` for its status. A sleepy device answers late: a newline wakes it and the command is repeated until
    `seconds`. Returns a dict (port, mac when the port name has one, answer, modules) or None when nothing answers."""
    try:
        c = Console(path).open()
    except Exception:
        return None
    try:
        end = time.time() + seconds
        while time.time() < end:
            answer = c.command("status", timeout=min(3.0, max(0.5, end - time.time())))
            if answer != "no reply":
                mac = MAC.search(path)
                return {"port": path, "mac": mac.group(1).lower() if mac else None, "answer": answer.strip(),
                        "modules": [{"name": n, "component": int(t), "installed": i == "true", "running": r == "true"}
                                    for n, t, i, r in MODULE.findall(answer)]}
            c.send("")
    finally:
        c.close()
    return None


def discover(seconds=10.0):
    """Every connected board that answers on its console."""
    return [d for d in (probe(p, seconds) for p in ports()) if d]


def pick(port=None, seconds=10.0):
    """The port to use: the given one, $XIP_PORT, or the only board that answers."""
    port = port or os.environ.get("XIP_PORT")
    if port:
        return port
    boards = discover(seconds)
    if len(boards) != 1:
        raise SystemExit("%s; use --port (tools/console.py list)" % ("no board answers" if not boards else "several boards answer"))
    return boards[0]["port"]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("cmd", choices=["list", "monitor", "send"])
    ap.add_argument("text", nargs="?", help="the command line for `send`")
    ap.add_argument("--port", help="serial device (default: the only board that answers, or $XIP_PORT)")
    ap.add_argument("--seconds", type=float, default=None)
    ap.add_argument("--wait", type=float, default=2.0, help="seconds to listen after `send`")
    ap.add_argument("--probe", type=float, default=10.0, help="seconds to wait for a sleepy board to answer when discovering")
    a = ap.parse_args(argv)
    if a.cmd == "list":
        for d in discover(a.probe):
            mods = " ".join(f"{m['name']}{'*' if m['running'] else ''}" for m in d["modules"] if m["installed"])
            print(f"{d['port']}  mac={d['mac'] or '-'}  modules: {mods or '-'}")
        return 0
    c = Console(pick(a.port, a.probe)).open()
    try:
        if a.cmd == "monitor":
            end = time.time() + a.seconds if a.seconds else None
            while end is None or time.time() < end:
                sys.stdout.write(c.read(1))
                sys.stdout.flush()
        elif a.cmd == "send":
            if not a.text:
                ap.error("send needs the command line")
            c.send(a.text)
            sys.stdout.write(c.read(a.wait))     # everything, logs included; Console.command() gives the answer alone
    except KeyboardInterrupt:
        pass
    finally:
        c.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
