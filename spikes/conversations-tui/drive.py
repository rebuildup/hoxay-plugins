#!/usr/bin/env python3
"""PTY driver for the rebuildup.conversations native TUI (Issue #293 visual evidence).

Spawns `plugins/conversations/bin/conversations tui` under a real PTY,
sends keystrokes, and snapshots the screen via pyte at each step.
Produces evidence/*.txt screen snapshots plus a raw ANSI typescript per
scenario.

The child env is scrubbed of every ambient HERDR_* variable, then
HERDR_SOCKET_PATH is pinned to the socket named by CONV_TUI_SOCKET so
the TUI targets the isolated verification session (never the live one).
XDG_CONFIG_HOME/XDG_STATE_HOME/XDG_DATA_HOME pass through unchanged so
the caller pins the isolated herdr/plugin dirs; HERDR_PLUGIN_STATE_DIR
may be set to point the plugin state at a seeded fixture copy.

Usage:
  CONV_TUI_SOCKET=/path/to/herdr.sock \
  XDG_CONFIG_HOME=... XDG_STATE_HOME=... XDG_DATA_HOME=... \
  /tmp/pyte-venv/bin/python spikes/conversations-tui/drive.py [scenario]

Requires pyte (`python3 -m venv /tmp/pyte-venv && /tmp/pyte-venv/bin/pip
install pyte`).
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

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.environ.get(
    "CONV_TUI_BIN", os.path.join(REPO, "plugins", "conversations", "bin", "conversations")
)
EV = os.path.join(HERE, "evidence")
COLS, ROWS = 100, 30


def child_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    socket_path = os.environ.get("CONV_TUI_SOCKET")
    if not socket_path:
        sys.exit("CONV_TUI_SOCKET is required (isolated herdr session socket)")
    env["HERDR_SOCKET_PATH"] = socket_path
    env["TERM"] = "xterm-256color"
    return env


class Session:
    def __init__(self, name):
        self.name = name
        self.master, slave = pty.openpty()
        self.set_size(COLS, ROWS)
        self.proc = subprocess.Popen(
            [BIN, "tui"], stdin=slave, stdout=slave, stderr=slave,
            env=child_env(), close_fds=True,
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

    def send(self, s, settle=0.25):
        os.write(self.master, s.encode())
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
        marked = []
        for y in range(self.screen.lines):
            row = self.screen.buffer[y]
            if any(row[x].bg not in ("default", None) for x in range(self.screen.columns)):
                marked.append(y)
        with open(out, "w") as f:
            f.write(
                f"# scenario={self.name} step={label} "
                f"size={self.screen.columns}x{self.screen.lines} bg_rows={marked}\n"
            )
            for line in lines:
                f.write(line + "\n")
        print(f"--- {self.name}/{label} (bg_rows={marked}) ---")
        for line in lines:
            print(line)

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
        print(f"exit status: {self.proc.poll()}")


def main():
    os.makedirs(EV, exist_ok=True)
    sc = sys.argv[1] if len(sys.argv) > 1 else "main"

    if sc == "main":
        s = Session("main")
        s.pump(1.2)  # boot: capability + sessions load
        s.snap("01-sessions")
        s.send("j")
        s.snap("02-selected")
        # Enter opens the session's normalized event log (history loader).
        s.send("\r", settle=0.1)
        s.pump(2.0)
        s.snap("03-events")
        s.send("j")
        s.snap("04-events-scroll")
        # Enter on an event row opens the detail overlay.
        s.send("\r", settle=0.3)
        s.snap("05-detail")
        s.send("\x1b", settle=0.3)  # Esc: detail returns to sessions list
        s.snap("06-back-to-sessions")
        # 's' search prompt: substring over indexed event text.
        s.send("s", settle=0.3)
        s.send("cache invalidation", settle=0.1)
        s.snap("07-search-draft")
        s.send("\r", settle=0.1)
        s.pump(2.0)
        s.snap("08-search-results")
        s.send("\x1b", settle=0.3)  # back to sessions
        # '/' filter over session list fields.
        s.send("/")
        s.send("claude", settle=0.1)
        s.snap("09-filter-draft")
        s.send("\r", settle=0.3)
        s.snap("10-filtered")
        # Clear the filter: '/' reopens with an empty draft; Enter applies
        # the empty query, restoring the full list.
        s.send("/")
        s.send("\r", settle=0.3)
        s.snap("11-filter-cleared")
        # Help overlay.
        s.send("?")
        s.pump(0.3)
        s.snap("12-help")
        s.send("?", settle=0.2)  # '?' toggles back
        # Confirm-gated reindex: dialog → 'y' → result.
        s.send("i", settle=0.4)
        s.snap("13-confirm-index")
        s.send("y", settle=0.1)
        s.pump(3.0)  # in-process index run
        s.snap("14-after-index")
        s.quit()

    else:
        sys.exit(f"unknown scenario: {sc}")


if __name__ == "__main__":
    main()
