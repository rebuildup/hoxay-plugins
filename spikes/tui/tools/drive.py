#!/usr/bin/env python3
"""PTY driver for the spike: spawns the binary in a real pty, sends
keystrokes/paste/mouse/resize, and snapshots the screen (via pyte) at each
step. Produces evidence/*.txt snapshots + a raw typescript per scenario.

Usage: drive.py <scenario>
"""
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

import pyte

BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "hoxay-tui-spike")
EV = os.path.join(os.path.dirname(__file__), "..", "evidence")
COLS, ROWS = 100, 30


class Session:
    def __init__(self, name):
        self.name = name
        self.master, slave = pty.openpty()
        # set window size -> kernel sends SIGWINCH
        self.set_size(COLS, ROWS)
        env = dict(os.environ, SPIKE_STATS=os.path.join(EV, f"stats-{name}.txt"))
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave,
            env=env, close_fds=True,
        )
        os.close(slave)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.raw = bytearray()
        self.typefile = open(os.path.join(EV, f"{name}.typescript"), "wb")

    def set_size(self, cols, rows):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, duration=0.25):
        end = time.time() + duration
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], max(0, end - time.time()))
            if not r:
                break
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            self.raw += data
            self.typefile.write(data)
            self.typefile.flush()
            self.stream.feed(data.decode("utf-8", errors="replace"))

    def send(self, s, settle=0.2):
        os.write(self.master, s.encode())
        self.pump(settle)

    def send_bytes(self, b, settle=0.2):
        os.write(self.master, b)
        self.pump(settle)

    def display_lines(self):
        # pyte's Screen.display crashes on wide-char trailing cells; build
        # lines from buffer cells directly.
        lines = []
        for y in range(self.screen.lines):
            row = self.screen.buffer[y]
            parts = []
            for x in range(self.screen.columns):
                data = row[x].data
                if data:
                    parts.append(data)
            lines.append("".join(parts).rstrip())
        return lines

    def snap(self, label):
        out = os.path.join(EV, f"{self.name}-{label}.txt")
        lines = self.display_lines()
        # mark rows that carry a non-default background (selection/highlight)
        marked = []
        for y in range(self.screen.lines):
            row = self.screen.buffer[y]
            if any(row[x].bg not in ("default", None) for x in range(self.screen.columns)):
                marked.append(y)
        with open(out, "w") as f:
            f.write(f"# scenario={self.name} step={label} size={self.screen.columns}x{self.screen.lines} bg_rows={marked}\n")
            for line in lines:
                f.write(line + "\n")
        print(f"--- {self.name}/{label} (bg_rows={marked}) ---")
        for line in lines:
            print(line)

    def resize(self, cols, rows, settle=0.4):
        self.set_size(cols, rows)
        # pyte screen must be recreated at new size; replay raw to rebuild
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)
        self.stream.feed(bytes(self.raw).decode("utf-8", errors="replace"))
        self.pump(settle)

    def quit(self, settle=0.4):
        if self.proc.poll() is None:
            try:
                self.send("q", settle)
            except OSError:
                pass
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.typefile.close()


def main():
    os.makedirs(EV, exist_ok=True)
    sc = sys.argv[1] if len(sys.argv) > 1 else "basic"

    if sc == "basic":
        s = Session("basic")
        s.pump(0.5); s.snap("01-list")
        s.send("j"); s.send("j"); s.snap("02-selected")
        s.send("\r"); s.snap("03-detail")
        s.send("G"); s.pump(0.3); s.snap("04-detail-bottom")
        s.send("\x1b"); s.pump(0.3)
        s.send("?"); s.snap("05-help")
        s.send("q"); s.pump(0.2); s.send("q"); s.quit()

    elif sc == "filter-paste":
        s = Session("filter")
        s.pump(0.5)
        s.send("/")
        s.send("café")          # typed incl. precomposed + combining via paste below
        s.snap("01-filter-typed")
        # bracketed paste of hostile unicode
        s.send_bytes(b"\x1b[200~" + "絵文字🔍とカフェ\u0301".encode() + b"\x1b[201~")
        s.snap("02-filter-pasted")
        s.send("\x1b")
        s.send("q"); s.quit()

    elif sc == "confirm":
        s = Session("confirm")
        s.pump(0.5)
        s.send("j"); s.send("d"); s.snap("01-confirm")
        s.send("n"); s.pump(0.2); s.snap("02-cancelled")
        s.send("d"); s.send("y"); s.pump(0.3); s.snap("03-confirmed")
        s.send("\x1b"); s.send("q"); s.quit()

    elif sc == "async":
        s = Session("async")
        s.pump(0.5)
        s.send("r"); s.pump(0.35); s.snap("01-progress")
        s.send("\x03")          # Ctrl-C cancels op
        s.pump(0.4); s.snap("02-cancelled")
        s.send("r"); s.pump(1.5); s.snap("03-done")
        s.send("q"); s.quit()

    elif sc == "mouse":
        s = Session("mouse")
        s.pump(0.5); s.snap("01-list")
        # SGR click on row 4 (0-based content row ~2 => screen row 4)
        s.send_bytes(b"\x1b[<0;20;5M")
        s.pump(0.3); s.snap("02-after-click")
        s.send("\r"); s.pump(0.3); s.snap("03-detail")
        # wheel down x3 in detail
        for _ in range(3):
            s.send_bytes(b"\x1b[<65;50;10M")
        s.pump(0.3); s.snap("04-scrolled")
        s.send("\x1b"); s.send("q"); s.quit()

    elif sc == "resize":
        s = Session("resize")
        s.pump(0.5); s.snap("01-100x30")
        s.resize(60, 18); s.snap("02-60x18")
        s.resize(120, 40); s.snap("03-120x40")
        s.send("j"); s.send("\r"); s.pump(0.3); s.snap("04-detail-wide")
        s.send("\x1b"); s.send("q"); s.quit()

    else:
        print("unknown scenario", sc)


if __name__ == "__main__":
    main()
