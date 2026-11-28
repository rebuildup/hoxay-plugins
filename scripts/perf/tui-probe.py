#!/usr/bin/env python3
"""PTY probe for TUI first-frame vs data-usable latency (#287 baseline).

Spawns a command under a real PTY (winsize 120x40), timestamps:
  - first_byte_ms : first output bytes on the master fd (~ first frame)
  - data_usable_ms: first time accumulated output matches --marker
                    (~ "data usable" — caller picks a post-load string)
After the marker + --settle seconds it sends --quit-key and waits for exit.
Reports child max RSS via wait4 rusage (top-level process only).

Usage:
  python3 scripts/perf/tui-probe.py --label inbox-tui --runs 10 \
    --marker '\\d+ inbox' -- env XDG_STATE_HOME=... ./plugins/inbox/bin/inbox tui
"""

import argparse
import fcntl
import json
import os
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

_ANSI_RE = re.compile(
    r"\x1b\[[0-9;?]*[ -/]*[@-~]"      # CSI
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"  # OSC
    r"|\x1b[()][0-9A-B]"               # charset
    r"|\x1b[=>#][0-9]?"                # misc
    r"|\x1b."                          # other escapes
)


def strip_ansi(data: bytes) -> str:
    return _ANSI_RE.sub("", data.decode("utf-8", errors="replace"))


def _sample_pgid_rss(pgid, stop, result):
    """Sum RSS (KB) of every process in pgid, polled every ~30 ms."""
    peak = 0
    while not stop.is_set():
        try:
            out = subprocess.run(
                ["ps", "-eo", "pgid=,rss="],
                capture_output=True,
                text=True,
                timeout=2,
            ).stdout
            total = 0
            for line in out.splitlines():
                parts = line.split()
                if len(parts) == 2 and int(parts[0]) == pgid:
                    total += int(parts[1])
            if total > peak:
                peak = total
        except Exception:
            pass
        stop.wait(0.03)
    result.append(peak)


def run_once(cmd, cols, rows, marker_re, timeout_s, settle_s, quit_key,
             advance_re=None, advance_key=None, advance_delay_ms=0):
    pid, fd = os.forkpty()
    if pid == 0:
        os.environ.setdefault("TERM", "xterm-256color")
        os.execvp(cmd[0], cmd)
        os._exit(127)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    # Sample process-group RSS (child + grandchildren share the pgid).
    stop = threading.Event()
    rss_samples = []
    sampler = threading.Thread(
        target=_sample_pgid_rss, args=(pid, stop, rss_samples), daemon=True
    )
    sampler.start()

    t0 = time.monotonic_ns()
    buf = b""
    t_first = None
    t_marker = None
    t_advance = None
    advance_sent = advance_re is None
    settle_at = None
    quit_sent = False
    quit_deadline = None
    status = None
    ru = None
    deadline = t0 + int(timeout_s * 1e9)

    while True:
        now = time.monotonic_ns()
        if now > deadline:
            break
        effective_deadline = deadline
        if settle_at is not None:
            effective_deadline = min(effective_deadline, settle_at)
        if quit_deadline is not None:
            effective_deadline = min(effective_deadline, quit_deadline)
        wait_s = min((effective_deadline - now) / 1e9, 0.05)
        r, _, _ = select.select([fd], [], [], wait_s)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                chunk = b""
            if not chunk:
                break
            if t_first is None:
                t_first = time.monotonic_ns()
            buf += chunk
        # evaluate markers on every loop iteration (incl. select timeouts);
        # match against ANSI-stripped text since TUIs interleave escapes
        text = strip_ansi(buf)
        if t_marker is None and marker_re is not None and marker_re.search(text):
            t_marker = time.monotonic_ns()
            settle_at = t_marker + int(settle_s * 1e9)
        if (not advance_sent and advance_re is not None and advance_re.search(text)
                and time.monotonic_ns() - t0 >= advance_delay_ms * 1e6):
            try:
                os.write(fd, advance_key.encode())
            except OSError:
                pass
            t_advance = time.monotonic_ns()
            advance_sent = True
        # quit after settle window elapsed
        if settle_at is not None and not quit_sent and time.monotonic_ns() >= settle_at:
            try:
                os.write(fd, quit_key.encode())
            except OSError:
                pass
            quit_sent = True
            settle_at = None
            # give the app a few seconds to exit, then force-kill
            quit_deadline = time.monotonic_ns() + int(3e9)
        if quit_deadline is not None and time.monotonic_ns() > quit_deadline:
            break
        # reap if exited
        wpid, status, ru = os.wait4(pid, os.WNOHANG)
        if wpid == pid:
            break

    if status is None:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
        _, status, ru = os.wait4(pid, 0)

    t_end = time.monotonic_ns()
    stop.set()
    sampler.join(timeout=2)
    pgid_rss_kb = rss_samples[0] if rss_samples else None
    try:
        os.close(fd)
    except OSError:
        pass

    maxrss_kb = None
    if ru is not None and ru.ru_maxrss:
        # macOS reports bytes, Linux KB; 0 means "not reported".
        maxrss_kb = ru.ru_maxrss if sys.platform != "darwin" else ru.ru_maxrss // 1024

    exited = os.WIFEXITED(status) if status is not None else False
    code = os.WEXITSTATUS(status) if exited else None
    return {
        "first_byte_ms": round((t_first - t0) / 1e6, 1) if t_first else None,
        "advance_ms": round((t_advance - t0) / 1e6, 1) if t_advance else None,
        "marker_ms": round((t_marker - t0) / 1e6, 1) if t_marker else None,
        "total_ms": round((t_end - t0) / 1e6, 1),
        "marker_found": t_marker is not None,
        "exit_code": code,
        "maxrss_kb": maxrss_kb,
        "pgid_peak_rss_kb": pgid_rss_kb,
        "output_bytes": len(buf),
    }


def pct(sorted_vals, p):
    if not sorted_vals:
        return None
    idx = min(len(sorted_vals) - 1, max(0, -(-p * len(sorted_vals) // 100) - 1))
    return sorted_vals[idx]


def stats(vals):
    vals = sorted(v for v in vals if v is not None)
    if not vals:
        return {"n": 0}
    mean = sum(vals) / len(vals)
    var = sum((v - mean) ** 2 for v in vals) / len(vals)
    return {
        "n": len(vals),
        "min": round(vals[0], 1),
        "p50": round(pct(vals, 50), 1),
        "p95": round(pct(vals, 95), 1),
        "p99": round(pct(vals, 99), 1),
        "max": round(vals[-1], 1),
        "mean": round(mean, 1),
        "stdev": round(var ** 0.5, 1),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default="tui")
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--timeout", type=float, default=60.0, help="seconds")
    ap.add_argument("--settle", type=float, default=0.3, help="seconds after marker")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--marker", default=None, help="regex for data-usable")
    ap.add_argument("--advance-marker", default=None,
                    help="regex; when matched, send --advance-key once (e.g. open a submenu)")
    ap.add_argument("--advance-key", default="\r")
    ap.add_argument("--advance-delay-ms", type=int, default=0,
                    help="don't send advance key before this many ms")
    ap.add_argument("--quit-key", default="q")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args()

    cmd = args.cmd
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        ap.error("no command")

    marker_re = re.compile(args.marker) if args.marker else None
    advance_re = re.compile(args.advance_marker) if args.advance_marker else None

    for _ in range(args.warmup):
        run_once(cmd, args.cols, args.rows, marker_re, args.timeout, args.settle,
                 args.quit_key, advance_re, args.advance_key, args.advance_delay_ms)

    results = []
    for i in range(args.runs):
        try:
            r = run_once(
                cmd, args.cols, args.rows, marker_re, args.timeout, args.settle,
                args.quit_key, advance_re, args.advance_key, args.advance_delay_ms
            )
        except OSError as exc:
            print(f"run {i}: fork/pty error: {exc}", file=sys.stderr)
            continue
        results.append(r)
        print(f"run {i}: {json.dumps(r)}", file=sys.stderr, flush=True)

    summary = {
        "label": args.label,
        "command": " ".join(cmd),
        "runs": args.runs,
        "warmup": args.warmup,
        "pty": f"{args.cols}x{args.rows}",
        "first_byte_ms": stats([r["first_byte_ms"] for r in results]),
        "advance_ms": stats([r["advance_ms"] for r in results]),
        "data_usable_ms": stats([r["marker_ms"] for r in results]),
        "marker_found": sum(1 for r in results if r["marker_found"]),
        "exit_codes": sorted({r["exit_code"] for r in results}),
        "maxrss_kb": stats([r["maxrss_kb"] for r in results]),
        "pgid_peak_rss_kb": stats([r["pgid_peak_rss_kb"] for r in results]),
        "note": "first_byte = first PTY output; data_usable = first output matching --marker regex (post-load content); maxrss = top-level child only; pgid_peak_rss = summed RSS of process group sampled ~30ms (includes TUI-spawned CLI children, double-counts shared pages)",
    }
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
