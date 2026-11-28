# rebuildup.inbox

Work inbox for herdr agent panes — the standalone-plugin port of T3 Code's
settle / snooze / pin / archive / manual-ordering / restore organize model.

- Plugin ID: `rebuildup.inbox`
- Install: `herdr plugin install rebuildup/hoxay-plugins/plugins/inbox`
  (pre-release: append `--ref release-0-9-0` or a commit SHA)
- Runtime: a committed prebuilt native binary (`bin/inbox`, Rust — Issue
  #291, ADR-0035). No Bun or toolchain required to install or run.
  Verified platform: macOS arm64 (`platforms = ["macos"]` — honest until
  Linux/Windows binaries exist and are verified).

## What it does

- `settle` hides a pane's organize state until its `state_change_seq`
  advances or its agent session identity changes.
- `snooze` hides a pane until a wall-clock deadline (`--until`), waking early
  on the same generation signals.
- `archive` hides a pane as "filed away" — it never closes the pane, stops
  the agent, or touches the PTY; `unarchive` (or attention/a generation
  change) brings it back.
- `pin` sorts a pane to the top of the inbox without hiding it.
- `order` sets a manual rank within the pane's display group
  (`--first`/`--last`/`--before <pane>`/`--after <pane>`).
- `list` filters: `--attention`, `--organize settled|snoozed|archived|none`,
  `--search <text>` (title/cwd/pane_id), `--workspace <id>` — read-side
  only, never mutate state.
- `link`/`unlink`/`links` manage user-declared PR/Issue references
  (`<repo>#<n>`) per pane — explicit associations only, no forge-event
  auto-linking; stale links never attach to a new agent generation.
- `watch-pr` is an opt-in PR-update watcher: it polls declared `pr` links
  through the `rebuildup.git` plugin's public CLI (`pr view`/`comments`/
  `checks`) and restores attention — clears settle/snooze/archive marks —
  on qualifying transitions (merge/close, new failing check, merge
  conflict, new non-self review/comment). No daemon, no webhook; freshness,
  backoff, and stop conditions are part of the v1 contract
  (`docs/contracts/inbox-v1.md` → `watch-pr`).
- `restore` returns a pane to the inbox and clears all organize state
  (declared links are preserved).
- `list` emits the versioned JSON contract (`herdr-plugins.inbox/v1`) for TUI,
  CLI, agents, and external clients such as herdr-t3 — see
  `docs/contracts/inbox-v1.md`.
- Event hooks (`pane.agent_status_changed`, `pane.closed`) and a startup
  reconcile keep organize state consistent and refresh pane metadata tokens
  (`inbox_state`, `inbox_snoozed_until`, `inbox_pinned`).

Organize state is user-side UI state only: it never modifies herdr agent
execution state, and attention statuses (e.g. `blocked`) are never hidden.

## Commands

From a plugin checkout, the binary is `./bin/inbox` (installed plugins get
the same relative argv through the manifest):

```bash
./bin/inbox list [--session NAME] [--socket PATH]
./bin/inbox settle <pane_id>
./bin/inbox snooze <pane_id> --until <iso8601|+30m|+2h|+1d>
./bin/inbox restore <pane_id>
./bin/inbox pin <pane_id>
./bin/inbox unpin <pane_id>
./bin/inbox archive <pane_id>
./bin/inbox unarchive <pane_id>
./bin/inbox order <pane_id> --first|--last|--before <pane>|--after <pane>
./bin/inbox list --attention --organize settled --search <text> --workspace <id>
./bin/inbox link <pane_id> --pr <owner>/<repo>#<n> [--verify]
./bin/inbox unlink <pane_id> --issue <owner>/<repo>#<n>
./bin/inbox links <pane_id>
./bin/inbox reconcile
./bin/inbox transitions [--after <seq>] [--limit <n>]
./bin/inbox watch [--interval SECONDS]
./bin/inbox watch-pr [pane_id] [--interval SECONDS] [--once] [--git-root PATH]
./bin/inbox capability
```

Inside a manifest action or event hook, `<pane_id>` may be omitted; the
plugin resolves it from `HERDR_PANE_ID` / `HERDR_PLUGIN_CONTEXT_JSON`.

## Interactive TUI

The plugin ships an interactive terminal surface (`herdr-plugins.tui/v1`,
ADR-0031/0032/0033) inside the same binary:

- Pane entrypoint `main` (`[[panes]]`, placement `tab`) runs
  `./bin/inbox tui` — an inbox browser with attention states,
  `/`-filter, scope cycling, pin/order/archive, settle/snooze/restore,
  PR/Issue link management, the transition log, and explicit `reconcile`.
- Action `open` (`rebuildup.inbox.open`) opens that pane via
  `plugin.pane.open` and returns immediately — bind it yourself, e.g.:

  ```toml
  [[keys.command]]
  key = "prefix+o"
  type = "plugin_action"
  command = "rebuildup.inbox.open"
  description = "open inbox TUI"
  ```

The TUI is a view over the same CLI contract: opening is read-only
(`reconcile` never runs implicitly), the origin context is pinned at
launch, agent generation is revalidated before every mutation, and every
mutation goes through a confirmation dialog. Non-TTY invocations print a
plain-text list instead of faking a UI.

`watch` is an opt-in long-running reconciler (subscription + periodic
expiry). Without it or an event, snooze expiry materializes lazily on the
next command invocation — see `docs/specs/inbox.md` for the constraint.

`transitions` is a read-only page over the per-session organize-transition
log (ADR-0029): every settle/snooze/restore/clear is recorded with a
monotonic `seq`, and consumers drain it via `--after <seq>` —
`rebuildup.projects` uses it for `runOnSettle`/`stopOnSettle` triggers.

`watch-pr` is likewise opt-in: run it (or `watch-pr --once` per
invocation) only while you want forge polling. It reads PR state solely
via the installed `rebuildup.git` plugin's CLI — install that plugin (or
point `--git-root`/`config.json gitPluginRoot` at it) for watching to
work; without it every poll reports an explicit error with backoff.

## State

Durable state lives only under herdr's plugin state dir
(`HERDR_PLUGIN_STATE_DIR` at runtime; `~/.local/state/herdr/plugins/rebuildup.inbox/state.json`
for external clients). Entries are namespaced per session (hostname +
socket path) so organize state never leaks across sessions or hosts.
Config (`config.json`, e.g. `defaultSnooze`) lives under the plugin config
dir. `snooze` also detaches a one-shot wake process that restores the pane
at its deadline without a resident daemon (ADR-0005). Nothing is written
inside the installed source tree.

State files written by the previous TypeScript implementation (schema v2)
are read verbatim by the Rust port — settle/snooze/pin/archive marks,
tracker links, watch baselines, and the transition log all carry over.
