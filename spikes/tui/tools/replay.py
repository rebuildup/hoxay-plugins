#!/usr/bin/env python3
"""Replay a `script(1)` typescript capture through pyte and print the screen
as it was just before the app left the alternate screen (i.e. the last TUI
frame), optionally dumping every Nth distinct frame instead.

Usage: replay.py <capture> [cols] [rows]
"""
import sys
import pyte


def main() -> None:
    path = sys.argv[1]
    cols = int(sys.argv[2]) if len(sys.argv) > 2 else 80
    rows = int(sys.argv[3]) if len(sys.argv) > 3 else 24
    data = open(path, "rb").read().decode("utf-8", errors="replace")
    # The app exits the alternate screen on quit; freeze the display there.
    idx = data.find("\x1b[?1049l")
    if idx != -1:
        data = data[:idx]
    screen = pyte.Screen(cols, rows)
    stream = pyte.Stream(screen)
    stream.feed(data)
    for line in screen.display:
        print(line.rstrip())


if __name__ == "__main__":
    main()
