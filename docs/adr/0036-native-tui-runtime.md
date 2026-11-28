# ADR-0036: Native TUI runtime — ratatui/crossterm for plugin surfaces, Inbox as the first consumer

## Status

Accepted (design foundation for Program #286 native-performance track;
spike evidence: #289, `docs/research/native-tui-spike-289.md`; first
consumer: #291 Inbox vertical slice)

## Context

Program #286 unifies plugin implementation on Rust so the normal
distribution no longer requires Bun/Node. ADR-0032 previously selected a
custom zero-dependency TypeScript renderer (`packages/tui`) for the TUI
surface defined by `tui-v1`/ADR-0031/ADR-0033. That decision was correct
for the Bun era; the native track needs an equivalent renderer in Rust.

Issue #289 spiked a minimal prototype (`spikes/tui/`, ratatui 0.30 +
crossterm 0.29) against the `tui-v1` capability set. Results
(`docs/research/native-tui-spike-289.md`):

- All contract-level capabilities pass on macOS pty harness: list,
  search/input, detail+viewport, confirm dialog, cancellable async op,
  keyboard + SGR mouse, bracketed paste, resize, alternate-screen +
  full terminal restore (incl. panic path), non-TTY fallback.
- CJK/combining chars/ambiguous-width handled by `unicode-width`
  (UAX #11); emoji ZWJ/flags pass through at byte level — glyph-level
  rendering deferred to real-terminal RC checks.
- Latency: in-process first frame ~2–15 ms (budget 50 ms), key→draw
  p50 ~0.5 ms / p95 ≤1.9 ms (budget p95 16 ms) — wide headroom.
- Known gaps: ratatui-crossterm emits no synchronized-output (mode
  2026); IME/ConPTY/WSL2/SSH/tmux unverified (RC list in the research
  doc); the TS↔Rust shared layer must be re-established as a Rust crate.

This ADR partially supersedes ADR-0032 **for the native track only**:
`packages/tui` remains the implementation for plugins still on Bun
during coexistence; it is not deleted until the last consumer migrates.

## Decision

1. **Rust TUI runtime = ratatui + crossterm.** New native plugin UIs
   (starting with Inbox, #291) render through a shared Rust crate —
   provisionally `crates/tui` or `packages/tui-rs` per the Cargo
   workspace layout #286/#288 settle — that mirrors `packages/tui`'s
   module roles: app shell (screen stack, reserved keys, task runner),
   components (list/detail/input/confirm/help), renderer config
   (theme, color degradation), and safety helpers (untrusted-text
   sanitize, bounded output, escape/path checks).
2. **`tui-v1` contract is unchanged and remains normative.** Entrypoint
   manifest shape, placement policy, action naming, target-identity
   passing, state ownership, keymap/help conventions, and the §5 safety
   checklist apply identically to Rust panes. The contract never
   specified a language; only the `command` argv changes (native binary
   instead of `bun run`), which is a distribution concern owned by
   #288, not a contract change.
3. **Inbox minimal boundary.** The Inbox Rust binary hosts CLI and TUI
   in one executable: the TUI calls the plugin's domain/state layer
   in-process (per #286's "no self-CLI respawn" rule) while preserving
   every `tui-v1` guarantee — pinned origin context, liveness
   revalidation before mutation, confirm-gated irreversible ops,
   secret non-display, honest unsupported states, unchanged non-
   interactive CLI behavior and exit codes. Durable state stays in
   `HERDR_PLUGIN_STATE_DIR` via the existing `inbox/v1` contract; the
   TUI keeps ephemeral view state only.
4. **Shared UX at contract level during coexistence.** TS and Rust
   implementations expose identical reserved keys (`q`/`Esc`, `?`/`h`,
   `Ctrl-C` cancel-then-exit, `/` search), the same three-band
   status/content/help grid, the same 256-color + degradation theme
   rules, and the same honest-state behavior — so users cannot tell
   which runtime rendered a surface. Divergence requires a contract
   revision, not a per-plugin choice.
5. **Unverified platforms are RC gates, not assumptions.** IME
   committed-input behavior, ConPTY/Windows Terminal, WSL2, SSH, and
   tmux must be exercised on real environments before the RC; the
   research doc §10 is the checklist. Synchronized-output (2026)
   support is an explicit open item for the shared crate.

## Consequences

- Plugin TUI startup sheds the Bun prerequisite and interpreter warmup;
  measured headroom supports the program's first-frame/key-latency
  budgets.
- `packages/tui` and its 14 consumers keep working untouched until each
  plugin's Rust migration lands; both stacks implement the same
  contract so surfaces can migrate plugin-by-plugin.
- A new shared Rust crate becomes a `packages/`-equivalent shared
  library; AGENTS.md's "only genuinely shared code" rule applies — it
  must be extracted when the second native consumer exists, and Inbox
  (#291) is the first.
- Safety helpers (`sanitize`, bounded buffers, confirm gating) must be
  ported into the shared crate **before** Inbox mutations ship in Rust;
  the spike's inline sanitize is not the production pattern.
- The spike (`spikes/tui/`) is evidence, not product: its shortcuts
  (timer redraw, guessed mouse rects, single-file structure) must not
  be copied into `crates/`.
- Upstream/platform risks stay visible: mode-2026 flicker mitigation,
  IME edge cases, and terminal-specific width behavior are tracked in
  the research doc's RC list rather than silently assumed.
