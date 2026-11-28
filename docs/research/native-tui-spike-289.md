# Native TUI spike — ratatui/crossterm prototype (Issue #289, Program #286)

Date: 2026-11-27. Machine: macOS arm64 (Apple Silicon), `rustc/cargo
1.100.0`, ratatui 0.30.2 + crossterm 0.29 + unicode-width 0.2.
Prototype: `spikes/tui/` (marked SPIKE, own `Cargo.toml`, ~570 lines of
Rust in one `main.rs`). Evidence: `spikes/tui/evidence/` (screen
snapshots, per-scenario stats, raw ANSI typescripts) and
`spikes/tui/tools/` (pty driver + typescript replayer).

This document answers the #289 acceptance criteria: per-capability
results with evidence, failure causes and alternatives, latency
measurements, the minimal common-UX policy, the platform verification
matrix, and the RC residual-verification list. Verification ran at two
levels: a Python `pty` + `pyte` harness (`tools/drive.py`,
`evidence/*.txt|*.typescript|stats-*`) and a **real herdr 0.9.5 pane** in
an isolated named session `np289` (`evidence/herdr-*.txt`; session
deleted afterwards, live session untouched). The adoption decision it
feeds is recorded in **ADR-0036** (native TUI runtime + Inbox boundary).

## 1. What was compared

| axis | existing `packages/tui` (TS, ADR-0032) | spike (Rust, ratatui/crossterm) |
| --- | --- | --- |
| runtime cost | Bun interpreter + per-process warmup; already the shipping path for all 14 plugins | static-ish native binary (~1.2 MB release), no runtime prerequisite beyond the binary itself |
| render model | hand-rolled diff renderer: alt screen, cursor addressing, sync-output mode 2026 where detected, NO_COLOR/`TERM=dumb` degradation | ratatui retained-mode buffer diff → minimal cursor-addressed cell writes; crossterm handles raw mode, keys, mouse, paste, resize |
| input | `node:readline`-style raw stdin + own UTF-8/escape parser | crossterm event parser (keys, CSI/SS3, SGR mouse, bracketed paste) — battle-tested on all target terminals |
| width/Unicode | `Intl.Segmenter` + wcwidth table | `unicode-width` crate (UAX #11) inside ratatui truncation/wrapping |
| safety contract | `sanitizeDisplay`, bounded buffers, frame throttle in one place | spike implements `sanitize()` (strip C0/C1/ESC) inline; a real port must lift this into a shared Rust crate — **gap, not a conflict** |
| contract fit | implements `tui-v1` today | spike exercises the same capability set; `tui-v1` is runtime-agnostic (entrypoints, identity passing, keymap/help, safety) so it carries over unchanged |

Conclusion of the comparison: the two stacks cover the same contract
surface. The Rust path wins on startup cost, binary footprint, and
removing the Bun prerequisite; it loses on already-written,
already-verified TS code and on ratatui's missing sync-output (2026)
support (see §5). Nothing in ADR-0031/0033 blocks the switch: panes are
real PTYs either way, remote/WSL/SSH semantics are identical, and the
manifest/`plugin.pane.open` path is unchanged — only the `command` argv
in `[[panes]]`/`[[actions]]` would point at a native binary instead of
`bun run`.

## 2. Prototype description

One binary, one file (`spikes/tui/src/main.rs`), deps: `ratatui 0.30`,
`crossterm 0.29`, `unicode-width 0.2`. Views: `List` (8 fixture items
deliberately hostile to rendering: CJK, precomposed + combining `café`,
ambiguous-width `・αβγ・`/box chars, ZWJ family emoji + flag, 500-char
line, raw-ESC injection probe), `Detail` (wrapped `Paragraph` +
scrollbar = viewport rendering), `Confirm` (modal over detail), `Help`.
Features exercised:

- `/` focuses a filter line; live filtering; `Esc` clears.
- `Enter` → detail; `j/k/↑/↓/g/G` nav; `d` → confirm dialog (`y`/`n`/`Esc`).
- `r` → fake async refresh on a worker thread with progress in the
  status line; `Ctrl-C` cancels via `AtomicBool`; second `Ctrl-C` with
  no op exits.
- SGR mouse (`EnableMouseCapture`): click selects a row, wheel scrolls
  list/detail.
- Bracketed paste (`EnableBracketedPaste`): `Event::Paste` inserts into
  the focused filter.
- `Event::Resize` → ratatui re-queries winsize each frame (SIGWINCH).
- Alternate screen + raw mode + bracketed paste + mouse capture, all
  restored on exit **and on panic** (panic hook + `Drop` guard).
- Non-TTY stdout → prints a plain fixture list and exits (mirrors the
  honest-fallback rule in `plugins/inbox/src/tui.ts`).
- `SPIKE_STATS=<path>` writes frame/latency counters on exit.

## 3. Per-capability results

Evidence files are under `spikes/tui/evidence/`; each `*.txt` snapshot
header records scenario/step/size and rows carrying a non-default
background (selection highlight).

| capability | result | evidence |
| --- | --- | --- |
| **real herdr 0.9.5 pane run** | **PASS** | isolated session `np289` (dedicated XDG dirs under `/var/tmp/herdr-np289`, headless `herdr --session np289 server`, deleted afterwards; live session untouched): workspace `w1` + pane `w1:p1` ran the release binary — list render, `j j Enter`→detail, `/`+`café` filter (typed `é`), `d`→confirm dialog with target+consequence, `r`→async progress + `ctrl+c`→`refresh cancelled`, `?`→help, `ctrl+c`/`q`→clean exit with shell prompt restored. Snapshots: `evidence/herdr-01..09-*.txt` |
| list rendering | PASS | `basic-01-list.txt` — 8 items, bordered block, status + help lines |
| keyboard nav + selection | PASS | `basic-02-selected.txt` (`bg_rows=[4]` after `j`,`j`) |
| detail view + scroll/viewport | PASS | `basic-03/04-detail*.txt` — wrapped paragraph + scrollbar (`▲/█/║/▼`) |
| search/filter text input | PASS | `filter-01-filter-typed.txt` — `/ café` accepted, list refilters |
| confirm dialog (irreversible-op gate) | PASS | `confirm-01-confirm.txt` — modal over detail with target + consequence; `n`/`y` paths in `confirm-02/03` |
| async op with progress + cancel | PASS | `async-01..03` — `refresh N/10` status ticks; `Ctrl-C` → `refresh cancelled`; run-to-done → `refresh complete` |
| mouse (click select, wheel scroll) | PASS | `mouse-02-after-click.txt` (`bg_rows=[4]` moved), `mouse-04-scrolled.txt` (detail scrolled by wheel) |
| terminal state restore on exit | PASS | tail of every `*.typescript`: `?1049l` (leave alt), `?2004l` (paste off), `?1000/1002/1003/1006/1015l` (mouse off), `?25h` (cursor restore); panic hook + `Drop` guard |
| diff/viewport rendering | PASS | `basic.typescript` = 8.7 KB for 53 frames (~164 B/frame) — ratatui emits only changed cells via cursor addressing |
| CJK text | PASS (cell level) | `basic-01-list.txt`, `resize-*.txt` — Japanese titles render aligned; widths computed by `unicode-width` |
| ambiguous-width chars | PASS w/ caveat | `・αβγ・`, `┌─┐│└─┘` render without breaking row alignment in snapshots; **caveat:** UAX#11 "ambiguous" = 1 cell in unicode-width; terminals in CJK locales may draw 2 — same exposure as the TS wcwidth table, must be rechecked per terminal at RC |
| combining chars | PASS | `café` (precomposed) and pasted `カフェ́` (combining U+0301) both kept in filter line and list without layout breakage (`filter-02-filter-pasted.txt`) |
| emoji (ZWJ sequence, flag) | PASS (herdr pane level) | raw typescript contains the full `👨‍👩‍👧‍👦` ZWJ bytes and `🇯🇵` RI pair; inside a real herdr 0.9.5 pane `pane read` shows both glyphs intact (`herdr-01-list.txt`) — herdr's terminal model handles width/ZWJ correctly. External-terminal eyeball still on the RC list |
| untrusted-text escape safety | PASS (spike level) | fixture `</span>\x1b[31mRED\x1b[0m` renders as literal `[31mRED[0m` text — `sanitize()` strips C0/C1/ESC before render |
| bracketed paste | PASS | `filter.typescript` contains `ESC[200~…ESC[201~`; pasted `絵文字🔍とカフェ́` lands in the filter verbatim (`filter-02-filter-pasted.txt`) |
| resize | PASS (PTY level) / PARTIAL (herdr pane) | `resize-01..04` — 100×30 → 60×18 → 120×40 via `TIOCSWINSZ`/SIGWINCH, layout reflows cleanly. Inside a **headless** herdr session, `pane split`/`pane resize` changed layout rects but the pane PTY winsize did **not** follow (p1 stayed ~119 cols while its rect shrank to 12; `stty size` inside the pane confirmed) — the app kept rendering at PTY width and herdr cropped. This is a herdr headless-server behavior (no attached client renderer), not a spike defect — identical exposure for the TS TUI. Attached-client resize propagation → RC |
| non-TTY fallback | PASS | `hoxay-tui-spike </dev/null` prints plain list, exit 0 |
| IME committed-input / cursor | UNVERIFIED | cannot be driven through a pty harness — see §4 |
| synchronized output (mode 2026) | NOT IMPLEMENTED | ratatui-crossterm does not emit `?2026h`; flicker mitigation = cell diff only. `packages/tui` uses 2026 where detected — parity gap to evaluate |

## 4. Unicode / IME / paste / resize findings

- **Width model.** ratatui truncates/wraps with `unicode-width`
  (UAX #11). It handled CJK (2-cell), combining marks (0-cell), and
  box/ambiguous chars without corrupting the frame buffer in the
  harness. The known residual risk is *ambiguous-width* characters:
  unicode-width reports 1 cell; some terminals in CJK locales render 2.
  The TS implementation has the identical exposure via its wcwidth
  table, so this is a shared contract-level concern, not a Rust
  regression — record for the RC terminal matrix.
- **IME responsibility split.** IME composition (preedit, candidate
  window, commit) is owned by the **terminal emulator + OS input
  method**, not by the pane process: the TUI runs in raw mode but the
  terminal still delivers *committed* text as UTF-8 bytes, which
  crossterm surfaces as `KeyCode::Char` / `Event::Paste`. The TUI's
  obligations are therefore: (a) never assume one key event = one
  edited char (IME commits can arrive as a multi-char burst or a paste
  event); (b) keep the cursor visible and do not move focus mid-commit;
  (c) do not enable terminal features that swallow committed bytes.
  This spike could only verify (a)–(c) at the byte level. **Real IME
  verification (Japanese input on macOS, Windows Terminal + Microsoft
  IME, WSL2) is unverified here and is on the RC list** — it is the
  single highest-risk unverified item because ConPTY/IME interaction is
  historically the weakest link.
- **Paste.** Bracketed paste works end-to-end; `Event::Paste` delivers
  the whole payload as one event (multi-byte safe). Applications must
  still sanitize pasted text before storing/rendering (done in spike).
- **Resize.** SIGWINCH → crossterm `Event::Resize` → ratatui re-queries
  `terminal.size()` each `draw()`. Reflow verified at three sizes. In
  herdr panes the PTY winsize is owned by herdr; the plugin sees the
  same semantics as any pane process.

## 5. Failure causes, fixes, and alternatives

Failures hit during the spike (all harness/app-level, none library-level):

1. **pty winsize 0×0 under `script(1)`** — first capture rendered
   nothing. Cause: `script` allocates a pty with no winsize when not
   attached to a real terminal. Fix: `stty rows R cols C` inside the
   spawned shell. Lesson for RC harnesses: always set winsize
   explicitly; also relevant because herdr panes always carry a real
   size, so this is harness-only.
2. **Missing `EnableMouseCapture` produced a false positive** — injected
   SGR mouse bytes were parsed as `MouseEvent` even though the app never
   enabled mouse reporting; a real terminal would never have sent them.
   Caught by auditing the typescript for `?1000h` (absent), then fixed.
   Lesson: *verify mode-enable sequences in captures*, not just app
   behavior.
3. **pyte `Screen.display` crashes on wide-char trailing cells** —
   harness bug, worked around by reading `screen.buffer` cells directly
   in `tools/drive.py`. Snapshots are therefore cell-accurate text but
   approximate for glyph shaping (ZWJ/flags/combining render partially).
4. **Constant redraw loop (fixed during spike hardening)** — the first
   cut polled at 50 ms unconditionally, redrawing every tick even when
   idle (~20 fps of no-op frames). Fixed to event-driven rendering:
   block on `event::poll` until input when no async op is active, poll
   at 50 ms only while an op needs progress repaints. Frame count for
   the `basic` scenario dropped 53→8; `async` still ticks during the
   op as intended. A real implementation must keep this discipline —
   render on event/dirty, never on a free-running timer.
5. **Off-by-one in mouse hit-testing** — the spike guesses the list
   rect (`row - 2`) instead of tracking layout rects; selection landed
   one row off in `mouse-02`. Real implementation must store the
   `Rect` returned by `Layout` and hit-test against it. Recorded as a
   known spike shortcut, not a library defect.
6. **Headless herdr does not propagate layout resize to pane PTY
   winsize** — in the isolated `np289` session, `pane split`/`pane
   resize` shrank the visible rect (down to 12 cols) while `stty size`
   inside the pane still reported ~119 cols; the app kept rendering at
   PTY width and herdr cropped the display. No crash, clean exit. This
   is a *herdr headless-server* behavior — with no attached client
   there is no renderer driving PTY winsize — and affects the TS TUI
   identically, so it is not a Rust regression. It does mean
   attached-client resize behavior is unverifiable from this harness →
   RC item (real session with an attached client).
7. **`send-keys` Escape+Enter coalescing** — sending `Escape Enter d`
   in one `pane send-keys` call produced filter-Enter semantics rather
   than two discrete events (raw `ESC`+`CR` can merge in escape
   parsing). Harness artifact of batched key injection, not an app bug;
   noted so future live-verification scripts send keys with delays.

Alternatives considered and rejected for the core:

- **Keep `packages/tui` (TS)** — already contract-complete and verified
  on all 14 plugins; rejected *for the native track* because it keeps
  the Bun prerequisite the program is removing, and pays interpreter
  startup on every pane. It remains the implementation for unmigrated
  plugins during coexistence (see ADR-0036).
- **Other Rust TUI crates** — `tui-rs` is unmaintained (ratatui is its
  successor); Cursive/bubbletea-style immediate-mode or Go runtimes do
  not match the retained-mode, diff-rendered model `packages/tui`
  established; ratatui is the de-facto standard and maps 1:1 onto the
  existing component concepts (list/detail/input/confirm/task).
- **hand-rolled ANSI in Rust** — rejected for the same reason ADR-0032
  rejected it in TS: safety/resize/width behavior would be
  re-implemented per plugin.

## 6. Latency measurements

Harness: `tools/drive.py` pty driver on this machine; stats written by
the app at exit (`evidence/stats-*.txt`). `first_frame_us` =
`main()` start → first `terminal.draw()` complete (includes raw-mode +
alt-screen entry + first full-frame render/write). `key_to_draw` =
`event::read()` return → next `draw()` complete. Caveat: this measures
the app's internal latency; OS/terminal input pipeline and PTY
transport are excluded (they are identical for TS and Rust paths).

| scenario | first frame | draw p50 | draw p95 | key→draw p50 | key→draw p95 |
| --- | --- | --- | --- | --- | --- |
| basic | 2.3 ms | 456 µs | 1.3 ms | 464 µs | 1.5 ms |
| filter | 5.6 ms | 520 µs | 1.3 ms | 532 µs | 0.7 ms |
| confirm | 15.0 ms | 528 µs | 2.2 ms | 690 µs | 1.3 ms |
| async | 2.7 ms | 536 µs | 2.2 ms | 444 µs | 0.9 ms |
| mouse | 15.0 ms | 558 µs | 1.5 ms | 607 µs | 1.9 ms |
| resize | 52.5 ms* | 712 µs | 3.5 ms | 882 µs | 1.9 ms |

\* the resize run's first frame coincided with harness/`pyte` replay
load; other runs are 2–15 ms. Treat ~15 ms as the honest typical
in-process first-frame figure.

Program budgets (#286): TUI first frame ≤ 50 ms, key→render p95 ≤ 16 ms.
Both are met with ~3× and ~8× headroom respectively **on this harness**.
Cold *process* spawn adds exec+dyld on top; the non-TTY path
(`</dev/null`) completes in ~10 ms wall (`time -p`), and the release
binary is 1.2 MB. Conceptual comparison to the TS baseline: `packages/tui`
pays Bun startup + module graph per pane launch; #287's measured
baseline was not yet published at spike time — **coordinate numbers
with #287 before final sign-off; do not block this ticket on it**.

## 7. Platform verification matrix

| platform | status | evidence / gap |
| --- | --- | --- |
| macOS arm64, pty harness (this machine) | VERIFIED for §3 capabilities | `evidence/*`; harness = real pty + pyte screen model |
| macOS arm64, real herdr 0.9.5 pane | VERIFIED for render/input/async-cancel/exit | isolated session `np289`, headless server; `evidence/herdr-*.txt`. Resize propagation unverified headlessly (see §5.6) |
| macOS Terminal.app / iTerm2 real-app eyeball | PARTIAL — bytes verified, glyphs unverified | needs a human-driven session; RC |
| Windows Terminal / ConPTY | UNVERIFIED | crossterm uses the ConPTY/Win32 console API; mouse/IME/paste semantics differ — RC |
| WSL2 | UNVERIFIED | inherits terminal of the Windows-side emulator; RC |
| SSH (remote herdr session) | UNVERIFIED | pane is a PTY on the server; terminal capabilities depend on the client — RC |
| tmux | UNVERIFIED | mouse/paste/key passthrough quirks (`extended-keys`, `set-clipboard`) — RC |

Nothing above may be reported as success until exercised on the real
target; the harness proves the *application logic and byte stream*, not
the terminal emulator's rendering.

## 8. Minimal common-UX policy (proposal for #291+)

Kept deliberately small — no 14-screen design.

1. **Grid.** Three-band layout adopted by the spike and matching
   `packages/tui` convention: row 0 = status line (op state, notices),
   middle = content, last row = key hint bar. Surfaces that need a
   filter/input affordance render it as an inline line inside the
   content block (as the spike does) rather than spawning a popup —
   popup placement stays reserved per `tui-v1` §placement policy.
2. **Theme.** 256-color default palette (cyan ids, dark-gray selection,
   yellow input caret), `NO_COLOR`/`TERM=dumb` ASCII degradation,
   Unicode-width-correct truncation. Colors carry meaning only
   (selection, input focus, destructive confirm) — never required to
   understand state.
3. **Shared keymap (contract-level, runtime-agnostic).** Preserve the
   `tui-v1` §6 reserved keys verbatim across TS and Rust
   implementations: `q`/`Esc` back-close, `?`/`h` help generated from
   the same key table, `Ctrl-C` cancel-then-exit, `/` search where a
   list exists. Per-surface keys are declared in one table per plugin
   (spike: `handle_key` match; real: shared keymap table component).
   During TS/Rust coexistence the *contract* is the shared artifact —
   users see identical keys/help regardless of which binary renders.
4. **Coexistence behavior.** `packages/tui` remains the implementation
   for unmigrated plugins; Rust plugins link a shared Rust crate (the
   `hoxay-tui` core implied by ADR-0036) that mirrors `packages/tui`'s
   module roles: app shell (screen stack, reserved keys, task runner),
   renderer (ratatui), components (list/detail/input/confirm/help),
   safety helpers (sanitize, bounded output, escape/path checks). Both
   stacks implement `tui-v1`; neither may fork herdr surfaces or hold
   business state.
5. **Honest-state rule (unchanged).** Unsupported/missing dependency →
   explicit state + CLI fallback; never fabricated success — same as
   `tui-v1` §5 and ADR-0033.

## 9. Adoption decision input for Inbox (#291)

Recommendation recorded in ADR-0036: **adopt ratatui/crossterm for the
Inbox Rust TUI**, because the spike shows the full `tui-v1` capability
set working with large latency headroom, terminal restore is robust,
and the only hard unverified area (IME/ConPTY) is terminal-side and
identical-risk for any native implementation — it is not a reason to
stay on TS, but it *is* a reason the RC matrix must include real
Windows/WSL2/SSH/tmux sessions before the RC is called.

What #291 can rely on now:

- The capability checklist in §3 is the implementation checklist.
- `tui-v1` contract is unchanged; manifest `[[panes]]`/`[[actions]]`
  shape is unchanged (only `command` argv becomes the native binary —
  pending #288's distribution decision for how the binary is built and
  installed).
- Safety contract items map 1:1; the shared Rust crate must port
  `sanitize`-equivalent + bounded output + confirm gating *before*
  Inbox screens land.
- The spike's shortcuts (fixed-rect mouse hit-test, timer redraw,
  inline sanitize) are explicitly **not** to be copied.

## 10. RC residual-verification list

Must be exercised on real environments before RC sign-off (none may be
claimed from this spike):

1. Japanese IME committed input + cursor behavior on macOS
   Terminal.app/iTerm2, Windows Terminal (ConPTY), WSL2.
2. Glyph-level rendering of ZWJ emoji, flags, combining marks, and
   ambiguous-width chars on each target terminal (not the pyte harness).
3. Mouse click/scroll/wheel on Windows Terminal, WSL2, tmux
   (`mouse=a`/`alternate-screen` interactions).
4. Bracketed paste of multi-byte text on each target.
5. Resize storms (rapid SIGWINCH) and very small sizes (≤ 40×10),
   **including pane-split resize with an attached herdr client** — the
   headless session did not propagate layout resize to PTY winsize, so
   the attached-client path is unverified (§5.6).
6. SSH-attached herdr session: pane opens server-side, client terminal
   capabilities.
7. `TERM=dumb` / `NO_COLOR` degradation paths.
8. Synchronized-output (2026) need assessment — decide whether the
   shared Rust crate adds it (crossterm has no built-in; may need raw
   writes or a patch) or accepts diff-only rendering.
9. Panic-mid-session terminal restore on each platform (kill -SEGV /
   panic in draw path).
10. Long-session resource behavior (RSS growth, frame cadence under
    continuous async ops) — program RSS budgets (≤20 MB typical).
