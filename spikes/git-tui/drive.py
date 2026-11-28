#!/usr/bin/env python3
"""PTY driver for the rebuildup.git native TUIs (Issue #292 visual evidence).

Spawns `plugins/git/bin/git tui` (main) and `plugins/git/bin/git tui forge`
under a real PTY, sends keystrokes, and snapshots the screen via pyte at
each step. Produces evidence/*.txt screen snapshots plus a raw ANSI
typescript per scenario.

Scenarios:
  main   — `git tui --cwd <scratch-fixture>`: home -> files -> diff ->
           log -> commit detail -> filter -> help -> confirm-gated stage.
           The fixture repo is created fresh in a tmp dir; the TUI only
           ever touches that scratch repo.
  forge  — `git tui forge` pinned to a pane of the ISOLATED herdr session
           named by GIT_TUI_SOCKET (np292). gh is intercepted by the
           committed stub in ./gh-stub so no real GitHub calls happen.

The child env is scrubbed of every ambient HERDR_* variable, then the
scenario re-pins exactly what it needs (HERDR_SOCKET_PATH for forge,
HERDR_TUI_PANE_ID for the forge origin). XDG_* pass through unchanged so
the caller pins the isolated herdr/plugin dirs.

Usage:
  GIT_TUI_SOCKET=/path/to/herdr.sock \
  GIT_TUI_FORGE_PANE=w2:p1 \
  XDG_CONFIG_HOME=... XDG_STATE_HOME=... XDG_DATA_HOME=... \
  /tmp/pyte-venv/bin/python spikes/git-tui/drive.py [main|forge]

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
import tempfile
import termios
import time

import pyte

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.environ.get("GIT_TUI_BIN", os.path.join(REPO, "plugins", "git", "bin", "git"))
GH_STUB = os.path.join(HERE, "gh-stub")
EV = os.path.join(HERE, "evidence")
COLS, ROWS = 110, 32


def base_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    env["TERM"] = "xterm-256color"
    # gh stub first on PATH so forge calls never reach the real gh.
    env["PATH"] = GH_STUB + os.pathsep + env.get("PATH", "")
    return env


def make_fixture():
    """Scratch git repo for the main TUI: 2 commits, dirty + untracked,
    a second branch. Never the real worktree."""
    d = tempfile.mkdtemp(prefix="git-tui-fixture-")

    def run(*a):
        subprocess.run(["git", *a], cwd=d, check=True,
                       env={**os.environ,
                            "GIT_AUTHOR_NAME": "tui", "GIT_AUTHOR_EMAIL": "t@e",
                            "GIT_COMMITTER_NAME": "tui", "GIT_COMMITTER_EMAIL": "t@e",
                            "GIT_AUTHOR_DATE": "2025-01-01T00:00:00Z",
                            "GIT_COMMITTER_DATE": "2025-01-01T00:00:00Z"},
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    run("init", "-q", "-b", "main")
    with open(os.path.join(d, "app.ts"), "w") as f:
        f.write("export const a = 1;\nexport const b = 2;\n")
    with open(os.path.join(d, "README.md"), "w") as f:
        f.write("# fixture\n")
    os.mkdir(os.path.join(d, "src"))
    with open(os.path.join(d, "src", "mod.ts"), "w") as f:
        f.write("export const x = 1;\n")
    run("add", ".")
    run("commit", "-qm", "initial commit")
    with open(os.path.join(d, "app.ts"), "a") as f:
        f.write("export const c = 3;\n")
    with open(os.path.join(d, "notes.txt"), "w") as f:
        f.write("scratch notes\n")
    run("add", "notes.txt")
    run("commit", "-qm", "add notes")
    run("branch", "feature-x")
    # Dirty state for the files/diff screens.
    with open(os.path.join(d, "README.md"), "a") as f:
        f.write("dirty line\n")
    with open(os.path.join(d, "untracked.txt"), "w") as f:
        f.write("untracked\n")
    return d


class Session:
    def __init__(self, name, argv, env):
        self.name = name
        self.master, slave = pty.openpty()
        self.set_size(COLS, ROWS)
        self.proc = subprocess.Popen(
            argv, stdin=slave, stdout=slave, stderr=slave,
            env=env, close_fds=True, cwd=REPO,
        )
        os.close(slave)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.raw = bytearray()
        self.typefile = open(os.path.join(EV, f"{name}.typescript"), "wb")

    def set_size(self, cols, rows):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, duration=0.3):
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
        try:
            os.write(self.master, s.encode())
        except OSError:
            return  # child already exited
        self.pump(settle)

    def display_lines(self):
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
        # 'q' pops one screen level; at the root it exits. Send up to 4.
        for _ in range(4):
            if self.proc.poll() is not None:
                break
            try:
                self.send("q", settle)
            except OSError:
                break
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.typefile.close()
        print(f"exit status: {self.proc.poll()}")


def scenario_main():
    fixture = make_fixture()
    print(f"fixture repo: {fixture}")
    s = Session("main", [BIN, "tui", "--cwd", fixture], base_env())
    s.pump(1.8)
    s.snap("01-home")
    s.send("j")
    s.snap("02-selected")
    s.send("k", settle=0.2)  # back to item 0: Changed files
    # Changed files: open the files list.
    s.send("\r", settle=0.1)
    s.pump(0.9)
    s.snap("03-files")
    s.send("j")
    s.snap("04-file-selected")
    # File diff.
    s.send("\r", settle=0.1)
    s.pump(0.5)
    s.snap("05-file-diff")
    s.send("\x1b", settle=0.3)
    s.snap("06-back-to-files")
    s.send("\x1b", settle=0.3)
    # Commit history: home cursor is still on item 0; move to index 2.
    s.send("j")
    s.send("j")
    s.send("\r", settle=0.1)
    s.pump(0.9)
    s.snap("07-log")
    s.send("\r", settle=0.1)
    s.pump(0.5)
    s.snap("08-commit-detail")
    s.send("\x1b", settle=0.2)
    s.send("\x1b", settle=0.3)
    # Filter the home list.
    s.send("/", settle=0.2)
    s.send("commit", settle=0.1)
    s.snap("09-filter-draft")
    s.send("\r", settle=0.3)
    s.snap("10-filtered")
    s.send("/", settle=0.2)
    for _ in range(len("commit")):
        s.send("\x7f", settle=0.02)
    s.send("\r", settle=0.3)
    s.snap("11-filter-cleared")
    # Help overlay.
    s.send("?", settle=0.3)
    s.snap("12-help")
    s.send("\x1b", settle=0.2)
    # Confirm-gated mutation: files -> stage the selected file -> confirm.
    s.send("\r", settle=0.1)  # home item 0 = Changed files
    s.pump(0.9)
    s.snap("13-files-again")
    s.send("j")
    s.send("s", settle=0.4)
    s.snap("14-confirm-stage")
    s.send("y", settle=1.4)
    s.snap("15-after-stage")
    s.send("\x1b", settle=0.3)
    s.quit()


def scenario_forge():
    socket_path = os.environ.get("GIT_TUI_SOCKET")
    if not socket_path:
        sys.exit("GIT_TUI_SOCKET is required (isolated herdr session socket)")
    pane = os.environ.get("GIT_TUI_FORGE_PANE", "w2:p1")
    env = base_env()
    env["HERDR_SOCKET_PATH"] = socket_path
    env["HERDR_TUI_PANE_ID"] = pane
    s = Session("forge", [BIN, "tui", "forge"], env)
    s.pump(1.2)
    s.snap("01-home")
    # Pull requests.
    s.send("\r", settle=0.1)
    s.pump(1.2)
    s.snap("02-prs")
    s.send("j")
    s.snap("03-pr-selected")
    s.send("k", settle=0.2)  # back to #7 so the detail matches the fixture
    s.send("\r", settle=0.1)
    s.pump(1.2)
    s.snap("04-pr-detail")
    s.send("\x1b", settle=0.3)
    s.send("\x1b", settle=0.3)
    # Issues.
    s.send("j")
    s.send("\r", settle=0.1)
    s.pump(1.2)
    s.snap("05-issues")
    s.send("j")
    s.send("k", settle=0.2)  # back to #5 so the detail matches the fixture
    s.send("\r", settle=0.1)
    s.pump(1.2)
    s.snap("06-issue-detail")
    s.send("\x1b", settle=0.3)
    s.send("\x1b", settle=0.3)
    # State filter cycle on the PR list ('s' cycles open/all/closed).
    # Home cursor is still on Issues (1); move up to Pull requests (0).
    s.send("k", settle=0.2)
    s.send("\r", settle=0.1)
    s.pump(1.0)
    s.send("s", settle=0.5)
    s.snap("07-pr-state-filter")
    s.send("s", settle=0.5)
    s.snap("08-pr-state-filter2")
    s.send("R", settle=0.8)
    s.snap("09-prs-reloaded")
    # Label filter prompt on issues.
    s.send("\x1b", settle=0.2)
    s.send("j")
    s.send("\r", settle=0.1)
    s.pump(1.0)
    s.send("l", settle=0.3)
    s.send("bug", settle=0.1)
    s.snap("10-label-draft")
    s.send("\r", settle=0.8)
    s.snap("11-issues-label-filtered")
    s.send("\x1b", settle=0.2)
    s.send("\x1b", settle=0.2)
    s.quit()


def main():
    os.makedirs(EV, exist_ok=True)
    sc = sys.argv[1] if len(sys.argv) > 1 else "all"
    if sc in ("main", "all"):
        scenario_main()
    if sc in ("forge", "all"):
        scenario_forge()


if __name__ == "__main__":
    main()
