#!/usr/bin/env python3
"""The board's serial console, from the computer: read what modules say, send them commands, reset the board.

  tools/console.py [BOARD] monitor [--seconds N]        print the console (Ctrl-C to stop); opening it does not reset the board
  tools/console.py [BOARD] send "status"                one command line, then print the answer (--wait seconds)

Resetting the board, including the five quick resets that start the loader, is esp-wifi-loader's reset.py.

Commands (`help` on the board lists them): status, start, stop, events, wifi, loader, help, reset, COMPONENT.METHOD key=value. The port comes from tests/device.toml.

Opening the port does not reset the board (the USB-Serial/JTAG block resets the chip only when DTR goes low while RTS
is high; this tool leaves the lines alone). What modules log() and emit() appears as lines like
  W (1234) xip: test: mod_test log
  W (1250) main: test event 0x7eff: pass=28 fail=0
"""
import argparse
import os
import re
import sys
import time
import tomllib

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")


LOG_LINE = re.compile(r"^\x1b?\[?[0-9;]*m?[IWEDV] \(\d+\) ")      # ESP-IDF log lines: "W (1234) tag: text"


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


def board_port(config, name):
    cfg = tomllib.load(open(config, "rb"))
    boards = [b for b in cfg["board"] if not name or b["name"] == name]
    if not boards:
        raise SystemExit(f"no such board in {config}")
    return boards[0]["port"]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("board", nargs="?", help="board name in tests/device.toml (default: the first)")
    ap.add_argument("cmd", choices=["monitor", "send"])
    ap.add_argument("text", nargs="?", help="the command line for `send`")
    ap.add_argument("--config", default=os.path.join(ROOT, "tests", "device.toml"))
    ap.add_argument("--seconds", type=float, default=None)
    ap.add_argument("--wait", type=float, default=2.0, help="seconds to listen after `send`")
    a = ap.parse_args(argv)
    if a.board in ("monitor", "send"):      # `console.py monitor` without a board name
        a.text, a.cmd, a.board = (a.cmd if a.cmd not in ("monitor", "send") else a.text), a.board, None
    c = Console(board_port(a.config, a.board)).open()
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


if __name__ == "__main__":
    sys.exit(main())
