# spikes/tui — native TUI spike (Issue #289)

**SPIKE — not shipped, not installed, not part of any plugin.** Evidence
source for the Rust TUI adoption decision under Program #286. Findings:
`docs/research/native-tui-spike-289.md`; decision record: ADR-0036.

Minimal ratatui + crossterm prototype covering the `tui-v1` capability
set: list, `/` filter input, detail view with scrollbar/viewport,
confirm dialog, one cancellable async op, keyboard + SGR mouse,
bracketed paste, SIGWINCH resize, alternate-screen + full terminal
restore (incl. panic path), non-TTY honest fallback.

## Build / run

```bash
cd spikes/tui
cargo build --release
./target/release/hoxay-tui-spike          # interactive (needs a TTY)
./target/release/hoxay-tui-spike </dev/null  # non-TTY: plain list, exits
SPIKE_STATS=/tmp/stats.txt ./target/release/hoxay-tui-spike  # writes frame/latency counters on exit
```

## Evidence harness

`tools/drive.py` (needs `pyte` + `wcwidth`; a venv was used during the
spike) spawns the binary in a real `pty`, sends keys/paste/mouse/resize,
and writes screen snapshots + raw typescripts to `evidence/`.
`tools/replay.py` replays a `script(1)` typescript through pyte.

`evidence/` holds the captured snapshots, stats, and raw ANSI streams
referenced by the research doc. Regenerate with:

```bash
./.venv/bin/python tools/drive.py basic        # etc: filter-paste confirm async mouse resize
```
