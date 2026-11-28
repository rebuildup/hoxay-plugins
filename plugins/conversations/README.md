# rebuildup.conversations

Provider transcript indexer for herdr panes
(`herdr-plugins.conversations/v1`).

## Install

```bash
herdr plugin install rebuildup/hoxay-plugins/plugins/conversations
```

Runtime: a committed prebuilt native binary (`bin/conversations`, Rust —
Issue #293, ADR-0035). No Bun or toolchain required to install or run.
Verified platform: macOS arm64 (`platforms = ["macos"]` — honest until
Linux/Windows binaries exist and are verified).

Prerequisites: herdr ≥ 0.9.0. `index`/`refresh`
additionally need a reachable herdr session (`--session`/`--socket` or
the default socket); `capability`, `sessions --offline`, `history`,
`search`, `show`, and `usage --offline` work without one (live-binding
checks degrade to `live: null`).

## What it does

Reads provider-owned session transcripts — never PTY text — and
normalizes them into structured events:

- **claude** (supported): `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`
  (honors `CLAUDE_CONFIG_DIR`).
- **codex** (supported): rollout JSONL under `$CODEX_HOME/sessions/**`
  (honors `CODEX_HOME`), matched by session id.
- **opencode** (supported): SQLite `opencode.db` under
  `$OPENCODE_DATA_DIR`/`$XDG_DATA_HOME/opencode`, opened read-only.

Every event carries the herdr identity observed at index time (host,
session, workspace/tab/pane, `agent_session`, `state_change_seq`)
plus `turn_index`; `assistant_message` events carry `model` + `usage`
token fields for #28.

Transcript resolution follows the T3 `usageTranscriptReader`
mechanism: `agent_session.value` (a session **id**, not a path) +
pane `cwd` + provider-known directories. `agent_session_path` is not
exposed by herdr's `AgentInfo` — recorded as an upstream gap in
ADR-0012.

Indexing is incremental: a byte-offset cursor plus an FNV-1a guard
detects in-place rewrites; only newline-terminated lines commit;
unterminated tails wait for the next scan (opencode rescans rewrite
its event log atomically each run). State and the append-only event
log live only under `HERDR_PLUGIN_STATE_DIR` (single-writer lock +
atomic rename).

## Usage

```bash
# from the plugin root (see `herdr plugin list --json` → plugin_root)
./bin/conversations capability
./bin/conversations index [--session NAME] [--socket PATH] [--timeout MS]
./bin/conversations refresh   # alias of index
./bin/conversations sessions [--offline]
./bin/conversations history (--pane PANE | --session-id ID) [--limit N] [--cursor OFF] [--offline]
./bin/conversations search <query> [--pane PANE] [--session-id ID] [--provider ID] [--type TYPE] [--limit N] [--cursor CUR] [--offline]
./bin/conversations usage [--pane PANE] [--session-id ID] [--provider ID] [--limit N] [--cursor CUR] [--offline]
./bin/conversations show <event-id|tool:ID|turn:N> [--pane PANE] [--session-id ID] [--provider ID] [--offline]
```

- `index`/`refresh` → `{indexed: [...], skipped: [...]}` — skipped
  entries carry explicit reasons (`unsupported_provider`,
  `unknown_provider`, `no_agent_session`, `transcript_missing`,
  `session_not_found`, `unsupported_layout`).
- `sessions` → indexed transcripts with identity binding, live
  binding, `stale`/`stale_reason` (`agent_session_changed` when a
  pane id was reused by a different agent generation), and read-time
  `transcript_present`.
- `history`/`search`/`usage` → bounded pages with opaque cursors;
  safe across index updates (append-only logs neither duplicate nor
  skip; rewritten logs are flagged via `restarted_sessions`).
- `search` → case-insensitive substring match over `text`,
  `tool_name`, `subtype`, `model`; results carry session/event
  references.
- `usage` → pages only events carrying provider `usage` (the read
  surface for #28; aggregation stays consumer-side).
- `show` → structured fetch by stable `event_id`, `tool:<id>`, or
  `turn:<n>`; missing transcripts/events are reported explicitly,
  never fabricated.

Full contract: `docs/contracts/conversations-v1.md`.
Spec: `docs/specs/conversations.md`. Design: ADR-0012.

## Interactive TUI

A read-mostly browser for the indexed transcripts, per
`docs/contracts/tui-v1.md` (ADR-0031/0032/0033):

```bash
herdr plugin pane open --plugin rebuildup.conversations --entrypoint main --placement tab
# or via the action:
herdr plugin action invoke rebuildup.conversations.open
```

Opening performs reads only (`capability` + `sessions`). Keys:

| key | action |
| --- | --- |
| `↑↓/jk`, `pgup/pgdn` | move / page |
| `/` | filter sessions |
| `enter` | session history (paged events) |
| `s` | search all indexed sessions |
| `i` | reindex — gated on `capability.mutation_commands`, confirm dialog required |
| `r` | reload sessions |
| `?`/`h`, `esc`/`q`, `ctrl+c` | help / back / cancel-exit (reserved) |

The `open` action places the UI in a `tab` via `plugin.pane.open`
(`HERDR_BIN_PATH`) and returns immediately; it never blocks on the UI.
Non-TTY invocations print the CLI equivalent and exit.
