# Contract: `herdr-plugins.inbox/v1`

Versioned structured interface for the `rebuildup.inbox` plugin. External
clients (herdr-t3, agents, scripts) MUST depend only on this contract, never
on the plugin's internal state file format.

## Invocation and discovery

The plugin is installed through the official herdr plugin mechanism:

```bash
herdr plugin install rebuildup/hoxay-plugins/plugins/inbox
# pre-release / pinned: herdr plugin install rebuildup/hoxay-plugins/plugins/inbox --ref <branch|tag|sha>
# development: herdr plugin link <repo>/plugins/inbox
```

The default ref is the repository's default branch; before the first release
merge, pass `--ref release-0-1-0` (or a commit SHA) explicitly.

An external client resolves the CLI as follows:

1. `herdr plugin list` (or the plugin registry) → installed plugin entry
   with `plugin_root` (managed checkout `…/checkout/plugins/inbox`, or the
   linked directory).
2. Run `<plugin_root>/src/cli.ts` with `bun run` from any cwd — imports are
   relative and self-contained within the checkout.
3. Alternatively invoke manifest actions (`settle`/`restore`/`snooze`) via
   `plugin.action.invoke` for focused-pane operations without arguments.

Since plugin v0.7.0 (#237) the plugin also ships an interactive terminal
surface under `herdr-plugins.tui/v1`: pane entrypoint `main`
(`[[panes]]`, placement `tab`) plus action `open`, which opens the
inbox browser via `plugin.pane.open`. The TUI is a view over this same
CLI contract — it performs no writes of its own, keeps the origin
context pinned at launch, revalidates agent generation before every
mutation, gates every mutation behind confirmation, and never runs
`reconcile` implicitly on open. See `docs/contracts/tui-v1.md`.

`capability` returns machine-readable discovery:

```json
{
  "contract": "herdr-plugins.inbox/v1",
  "plugin": "rebuildup.inbox",
  "version": "0.8.0",
  "commands": [
    "list", "settle", "snooze", "restore",
    "pin", "unpin", "archive", "unarchive", "order",
    "link", "unlink", "links",
    "reconcile", "transitions", "watch", "watch-pr", "capability"
  ],
  "mutation_commands": [
    "settle", "snooze", "restore",
    "pin", "unpin", "archive", "unarchive", "order",
    "link", "unlink",
    "reconcile", "watch", "watch-pr"
  ],
  "command_metadata": {
    "list":        { "kind": "read", "idempotent": true, "coalesce": true, "freshness": "event-driven", "invalidation": "pull" },
    "links":       { "kind": "read", "idempotent": true, "coalesce": true, "freshness": "event-driven", "invalidation": "pull" },
    "transitions": { "kind": "read", "idempotent": true, "coalesce": true, "freshness": "snapshot-consistent", "invalidation": "pull" },
    "capability":  { "kind": "read", "idempotent": true, "coalesce": true },
    "help":        { "kind": "read", "idempotent": true, "coalesce": true },
    "settle":      { "kind": "mutation", "idempotent": true },
    "snooze":      { "kind": "mutation", "idempotent": true },
    "restore":     { "kind": "mutation", "idempotent": true },
    "pin":         { "kind": "mutation", "idempotent": true },
    "unpin":       { "kind": "mutation", "idempotent": true },
    "archive":     { "kind": "mutation", "idempotent": true },
    "unarchive":   { "kind": "mutation", "idempotent": true },
    "order":       { "kind": "mutation", "idempotent": true },
    "link":        { "kind": "mutation", "idempotent": true },
    "unlink":      { "kind": "mutation", "idempotent": true },
    "reconcile":   { "kind": "mutation", "idempotent": true },
    "watch":       { "kind": "mutation" },
    "watch-pr":    { "kind": "mutation" }
  },
  "actions": ["open", "settle", "restore", "snooze", "pin", "unpin", "archive", "unarchive"],
  "tui": { "contract": "herdr-plugins.tui/v1", "entrypoint": "main", "action": "open" },
  "events": ["pane.agent_status_changed", "pane.closed"],
  "startup": ["reconcile"],
  "state_file": "…",
  "socket": null
}
```

## Target selection (Host / Session)

The CLI always talks to **the herdr server owning the socket it resolves**.
Explicit flags win over ambient environment; any contradiction between
explicit and ambient targets — or between the two ambient variables — is a
`target_conflict` error (exit 1), never a silent pick:

1. `--socket PATH` / `--session NAME` (explicit; local socket path — see
   Remote hosts). `--session NAME` resolves to
   `<config>/sessions/<name>/herdr.sock` (`default` → `<config>/herdr.sock`).
   When both flags are given they must agree on the same socket.
2. Ambient context, consistent pair required when both are set:
   `HERDR_SOCKET_PATH` wins over `HERDR_SESSION` →
   `<config>/sessions/<name>/herdr.sock`; if both are set they must resolve
   to the same socket.
3. default session socket `<config>/herdr.sock` — implicit fallback.

**Mutation gating**: `settle`, `snooze`, `restore`, `reconcile`, `watch`,
and `_wake` mutate plugin state and pane metadata, so they require an
explicit or ambient target — the implicit default socket is rejected with
`target_required` (exit 1). Read commands (`list`, `capability`) may use
the default fallback. The `mutation_commands` capability field (added in
plugin v0.6.0, #224 — additive) advertises the full set of state/herdr-
visible mutating commands: the organize verbs, `order`, `link`/`unlink`,
`reconcile`, `watch`, and `watch-pr`; `list`, `links`, `transitions`, and
`capability` are read-only.

`command_metadata` (added in plugin v0.8.0, #263 minimal subset — additive)
maps each command to `{ kind, idempotent?, coalesce?, freshness?,
invalidation? }`. `commands`/`mutation_commands` are unchanged; consumers
MUST treat the section as optional and fall back to conservative
defaults when absent. Daemon-class commands (`watch`, `watch-pr`) omit
`idempotent` — they are excluded from retry/coalesce semantics.

**Ambient pane identity**: when the pane id comes from `HERDR_PANE_ID` or
`HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id` (not an explicit positional),
the ambient `HERDR_WORKSPACE_ID`/`HERDR_TAB_ID` (or context
`workspace_id`/`tab_id`) must match the pane's `workspace_id`/`tab_id` in
the resolved session snapshot; a mismatch fails closed with
`ambient_identity_mismatch` before any metadata write. An explicit
positional pane id overrides the ambient pane id and is verified by
session membership alone.

Operating-system rules match herdr: unix socket on macOS/Linux, named pipe
`\\.\pipe\<path>` on Windows (handled inside the client).

**Remote hosts**: the socket API is local-only (unix socket / named pipe),
and plugin hooks, actions, and durable state always live on the herdr host.
The supported way to operate a remote herdr host is to run this CLI **on
that host** — locally in a shell, via the user's existing SSH/device routes,
or through `herdr --remote` / `herdr --machine` when the client supports
remote command execution. The CLI itself adds no remote transport, and
`--socket`/`HERDR_SOCKET_PATH` are intended for local sockets only.

Operating a remote host through a forwarded socket (`ssh -L`, `socat`) is
**not a supported path**: the state file is host-local, so a local CLI
pointed at a forwarded remote socket would resolve a separate organize
namespace keyed by the forwarded path — invisible to the remote plugin's
hooks and metadata projection, and split from the state the remote plugin
actually owns. Clients that need remote inbox data must execute the CLI on
the target host (or consume `watch` output relayed from it). `--session`
must name a session that exists **on the target host**; session names are
not globally unique.

## Commands

| Command | Args | Description |
| --- | --- | --- |
| `list` | `[--attention] [--organize K] [--search T] [--workspace ID]` | Inbox snapshot (below); filters are read-side only. |
| `settle` | `pane_id?` | Hide pane until generation change. |
| `snooze` | `pane_id? --until <spec>` | Hide pane until deadline; early wake on generation change. |
| `restore` | `pane_id?` | Clear all organize state (marks, pin, order). |
| `pin` | `pane_id?` | Mark pane pinned: sorts at the top of the inbox. Never hides. |
| `unpin` | `pane_id?` | Clear the pin mark. |
| `archive` | `pane_id?` | Hide pane as archived until generation change/attention/`unarchive`. Never touches the agent or pane. |
| `unarchive` | `pane_id?` | Clear the archive mark; other organize state is kept. |
| `order` | `pane_id? --first\|--last\|--before <pane>\|--after <pane>` | Set manual order rank within the pane's display group. |
| `link` | `pane_id? --pr <repo>#<n>` or `--issue <repo>#<n>` `[--verify]` | Declare a user-owned tracker link on the pane. |
| `unlink` | `pane_id? --pr <repo>#<n>` or `--issue <repo>#<n>` | Remove one declared link. |
| `links` | `pane_id?` | Read the pane's declared links (staleness resolved). |
| `reconcile` | — | Resolve expirations/generation changes, prune stale entries, repair metadata. |
| `transitions` | `--after <seq>` `--limit <n>` | Read-only page over the per-session organize-transition log (below). |
| `watch` | `--interval SEC` | JSONL event stream + periodic reconcile (below). |
| `watch-pr` | `[pane_id] [--interval SEC] [--once] [--git-root PATH]` | Opt-in PR-update watcher: polls declared PR links via the git plugin and restores attention on qualifying transitions (below). |
| `capability` | — | Discovery document (above). |

Global flags (after the command): `--session NAME`, `--socket PATH`,
`--timeout MS` (per-request timeout, default 10000). Global flags also
accept `--name=value` form.

Option parsing is strict: every `-`-prefixed token must be a flag the
command documents (`--until`/`--for` on `snooze`, `--until` on `_wake`,
`--interval` on `watch`); unknown options, `--flag=value` on boolean
flags, missing flag values, invalid values, and extra positionals are
usage errors (exit 2). A `--` separator ends option parsing.

`--until` accepts ISO 8601, `+30m`/`+2h`/`+1d`, or bare `30m`. Without
`--until`, `snooze` reads `defaultSnooze` from the plugin config dir's
`config.json` (used by the manifest `snooze` action, which takes no args).

`order` requires exactly one placement flag. Ordering is scoped to the
moved pane's display group — items sharing its `attention` and `pinned`
flags — because `list` sorts by those flags before `order`. The reference
pane of `--before`/`--after` must be a visible inbox item in the same
group; hidden panes cannot be ordered (restore/unarchive first).

`list` filters combine freely and never mutate state:

- `--attention` — only items with `attention: true`.
- `--organize settled|snoozed|archived|none` — items whose `organize.kind`
  matches; `none` selects items with no hide-kind mark (pin/order do not
  count). Hidden items remain visible to filters — filtering does not
  change `in_inbox`.
- `--search <text>` — case-insensitive substring over `title`, `cwd`, and
  `pane_id`.
- `--workspace <id>` — exact `workspace_id` match.

## Tracker links (`link` / `unlink` / `links`)

`link`/`unlink`/`links` manage **user-declared** associations between a
pane and a known forge item (`<repo>#<n>`, repo = `owner/name` or a deeper
forge path). They are explicit references stored in plugin state — never
derived from forge events (no webhook/auto-linking; PR-update attention
restore is a separate concern). `link` requires the pane to exist in the
session snapshot; re-linking the same `kind+repo+number` re-stamps the
identity fingerprint and is the explicit re-validation path. `--verify`
additionally runs `gh <kind> view` as an opt-in existence check; any
failure (offline, unauthenticated, not found) is reported in `warnings`
and the link is still recorded. The `gh` child runs under a sanitized
environment (`ghChildEnv` from `packages/git-core`, shared with the git
plugin): ambient `GH_DEBUG`/`GH_REPO`/`GH_FORCE_TTY` and repo-redirecting
`GIT_*` variables are stripped so debug logs (which can carry auth
headers) or env retargeting cannot reach the verify call, and gh's
stderr is never copied into `warnings[]` verbatim — untrusted child
output could otherwise smuggle credential markers into the result.

Links follow the same identity discipline as organize marks **minus timed
expiry**: they survive routine `state_change_seq` advances (status
transitions — a link describes work, not a transient status) but are
dropped when the `agent_session` fingerprint changes or the recorded seq
regresses (pane/agent identity recycled), on `pane.closed`, and by
retention pruning. `restore` clears organize state but preserves links.

`link` emits `{contract, command:"link", pane_id, link, links, warnings}`;
`unlink` emits `{contract, command:"unlink", pane_id, removed, links,
warnings}` (`removed` is the public link view); `links` emits
`{contract, command:"links", pane_id, links, warnings}`. All three use
only `session.snapshot` — no metadata projection, no execution surfaces.

## Output contract

- Success: exactly one JSON document on stdout (pretty-printed, except
  `watch` which is JSONL — one compact object per line).
- Failure: one JSON object on **stderr**,
  `{"contract","ok":false,"error":{"code","message"}}`, plus exit code
  `2` (usage: `"usage"` or `invalid_session`) or `1` (handled error:
  `"error"`, `target_conflict`, `target_required`,
  `ambient_identity_mismatch`). Socket/transport failures are errors —
  never an empty inbox.
- `pane_id` may be omitted when `HERDR_PANE_ID` or
  `HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id` is set (manifest actions,
  event-hook contexts).

## `list` output

```json
{
  "contract": "herdr-plugins.inbox/v1",
  "generated_at": "2026-01-10T12:00:00.000Z",
  "session": { "name": "default", "socket": null },
  "state_file": "/abs/path/state.json",
  "filters": { "attention": false, "organize": null, "search": null, "workspace": null },
  "counts": { "total": 0, "inbox": 0, "attention": 0, "organized": 0, "pinned": 0, "archived": 0, "linked": 0 },
  "items": [ { "pane_id": "…", "…": "…" } ]
}
```

`session.name`/`session.socket` echo the resolved target (explicit flag or
env); they are `null` when the default socket was used implicitly.
`filters` echoes the effective filter set (`null`/`false` = unset).

`counts` fields (all additive within v1) describe the **emitted**
(post-filter) items: `total` items reported, `inbox` items with
`in_inbox: true`, `attention` items flagged `attention: true`, `organized`
items carrying any hide-kind (`settled`/`snoozed`/`archived`), `pinned`
items with a live pin, `archived` items whose `organize.kind` is
`archived`, `linked` items carrying at least one tracker link.

Item fields: `organize` is `{kind, since, until?}` where `kind` is
`settled` | `snoozed` | `archived` (`until` on snoozed only); `pinned` is
a boolean; `order` is a number rank or absent; `links` is an array
(possibly empty) of `{kind, repo, number, ref, at}` — `kind` is `pr` |
`issue`, `ref` is the canonical `<repo>#<number>` display string, `at` the
ISO declaration time. `attention` (`blocked`) forces `in_inbox: true`
regardless of marks, and items for panes absent from the snapshot carry
only identity/organize/link fields with `in_inbox: false` (absent-pane
items are reported when they carry an organize view **or** links).

`items` sort order: `pinned` first, then `attention`, then `order`
ascending (items without `order` sort after ordered ones), then `pane_id`.
`order` is a layout hint only — it never hides an item and is not
generation-guarded; it is cleared by `restore`, `pane.closed`, and
retention pruning like every other organize field.

## Organize/mutate output

`settle`/`snooze`/`restore`/`pin`/`unpin`/`archive`/`unarchive` emit
`{contract, command, pane_id, agent_status, state_change_seq, organize,
pinned, order, warnings}`; `organize` is `null` when no hide-kind mark
survives (`archived` is a hide-kind and appears here), `pinned`/`order`
echo the pane's surviving pin mark and order rank. `order` emits
`{contract, command, pane_id, order, placement, warnings}` where
`placement` echoes the requested placement. `warnings` lists non-fatal
issues (e.g. metadata projection failure) — the state mutation itself
already succeeded; a later `reconcile`/`watch` repairs projections.
`restore` preserves declared tracker links (see Tracker links).

## `reconcile` output

```json
{
  "contract": "herdr-plugins.inbox/v1",
  "command": "reconcile",
  "resolved_at": "…",
  "cleared": ["pane-id"],
  "pruned": ["pane-id"],
  "projected": ["pane-id"],
  "warnings": []
}
```

- `cleared`: organize marks removed by expiry/generation/identity resolution.
- `pruned`: state entries dropped because their pane has been absent from
  the snapshot beyond the retention window (24h) or the entry cap (500).
- `projected`: panes whose metadata tokens were (re)written this pass —
  organized panes get `inbox_state`/`inbox_snoozed_until`/`inbox_pinned`,
  cleared panes get tokens cleared. Re-running `reconcile` is the repair
  path for any partial projection failure.

## `transitions` output (ADR-0029, #182)

Inbox records every real organize transition in a per-session log inside
the plugin state file (`transitions` key — internal, not part of this
contract). `transitions` is the read-only drain surface external consumers
use to observe settle/restore/clear activity; it never fabricates herdr
events and never exposes the state file directly.

```json
{
  "contract": "herdr-plugins.inbox/v1",
  "command": "transitions",
  "generated_at": "…",
  "session_key": "hostname:socket-path",
  "head_seq": 42,
  "oldest_seq": 7,
  "dropped": 3,
  "transitions": [ { "seq": 8, "…": "…" } ]
}
```

- `session_key` echoes the session namespace the page belongs to.
- `head_seq` is the per-session monotonic head (0 when no log exists);
  `oldest_seq` is the smallest retained `seq` (`null` when empty); `dropped`
  counts transitions evicted by the 500-per-session retention cap. A
  consumer whose stored cursor is below `oldest_seq`/`seq - dropped`
  window MUST treat it as expired — transitions were lost — rather than
  assume a gapless history.
- `transitions` items carry `{seq, at, kind, pane_id}` plus context fields
  captured from the same snapshot the mutating command already loaded:
  `workspace_id`, `cwd`, `agent_status`, `state_change_seq`,
  `agent_session` (fingerprint or `null`). `kind` is `settled` | `snoozed`
  | `restored` | `cleared`; `cleared` items add `cause`:
  `expired` | `generation` | `session_mismatch` | `pane_closed` | `pruned`.
- Flags: `--after <seq>` is an **exclusive** cursor (only `seq > after`);
  omit it to read the newest page (`--limit`, default 200, max 500). A
  consumer drains by storing `head_seq` after each batch; replaying a
  partially-processed batch is safe because `seq` is unique per session.
- `transitions` is read-only: expiry/generation resolution is the same
  deterministic in-memory path as `list`; nothing is persisted that `list`
  would not already resolve identically. Pin/order changes are layout
  hints and do **not** produce transitions.

## `watch` output and stop conditions

JSONL on stdout:

- `{"contract","type":"watch.started","at"}` once subscribed.
- `{"contract","type":"watch.reconcile","trigger":"event"|"interval",
  "event"?, "resolved_at","cleared","pruned","projected","warnings"}` per
  pass — on every `pane.agent_status_changed` subscription event and every
  `--interval` seconds (default 60).
- `{"contract","type":"watch.error","trigger","error"}` on a failed pass
  (watch keeps running).
- `{"contract","type":"watch.stopped","reason","at"}` on exit;
  `reason` is `signal` (SIGINT/SIGTERM, exit 0) or `stream_end`
  (subscription closed unexpectedly, exit 1).

`watch` is opt-in; nothing resident runs by default. Snooze expiry is
resolved lazily at every read anyway, so contract clients never observe a
stale "snoozed" state. Independently, each `snooze` detaches a one-shot
`_wake` process (internal, not part of this contract) that sleeps until the
deadline and runs one reconcile — this keeps pane metadata tokens fresh on
the herdr host without user action (ADR-0005). `watch`/startup reconcile
remain the repair paths for any missed wake.

## `watch-pr` output and semantics (added in plugin v0.5.0)

`watch-pr` is the opt-in PR-update attention watcher (Issue #178). It is a
plain opt-in process — **no resident daemon, no webhook** (webhook/inbound
delivery is an upstream gap, ADR-0026). When it is not running, PR updates
are simply not observed; nothing promises automatic attention return.

**Scope**: every declared `kind: "pr"` tracker link in the session (or only
the pane given as the optional positional `pane_id`). `issue` links are
not watched — the git plugin exposes no public issue-read surface; this is
a recorded limitation, not silent behavior. Only links that survive the
normal staleness rules (agent-session fingerprint, seq regression, live
pane) are polled.

**Poll source**: each watched PR is read exclusively through the
`rebuildup.git` plugin's **public CLI** (`git pr view`, `git pr comments`,
`git pr checks`) — never gh internals, never tokens, never the git
plugin's state files (ADR-0013 pattern). The git plugin root resolves via
`--git-root` → `config.json gitPluginRoot` → sibling `../git` →
`herdr plugin list --json`; the child's `HERDR_PLUGIN_*` env is scrubbed
and pinned to the git plugin's own dirs.

**Freshness and cost bounds**:

- Poll interval: `--interval SEC` (default 120, matching the T3 Code
  2-minute cadence). One tick polls each eligible link once (3 git-CLI
  reads per link); links are processed sequentially.
- Per-link freshness is recorded in plugin state (`watch` map):
  `last_checked_at` (last *successful* poll), `last_error`,
  `consecutive_failures`, `next_poll_at`. Failed polls never overwrite the
  last good observation — staleness is reported, never fabricated.
- Failure backoff: `interval × 2^failures` capped at 30 minutes; deferred
  links are skipped until `next_poll_at`.
- Rate-limit/unreachable gh surfaces as `watch-pr.error` events with
  `consecutive_failures`, `next_poll_at`, and the age of the last good
  `last_checked_at` — an explicit state, not silence.

**JSONL events** (stdout, one compact object per line):

- `{"type":"watch-pr.started","interval_ms","pane_filter","once",
  "git_root","git_root_via","git_error","at"}` once at startup.
- `{"type":"watch-pr.poll","pane_id","ref","ok":true,"baseline",
  "triggers","new_failing_checks","new_review_count","new_comment_count",
  "terminal","observation","last_checked_at","at"}` per successful poll.
  `baseline: true` means this poll established the first observation (no
  diff is computed — linking an already-merged/closed PR records
  `terminal` silently and cannot fabricate attention). `observation`
  summarizes `{state, is_draft, merge_state_status, mergeable, updated_at,
  failing_checks, review_count, comment_count}`.
- `{"type":"watch-pr.error","pane_id","ref","ok":false,"error",
  "consecutive_failures","next_poll_at","last_checked_at","at"}` per
  failed poll.
- `{"type":"watch-pr.attention","pane_id","triggers","links":[{ref,
  triggers, new_failing_checks, new_review_count, new_comment_count}],
  "cleared","warnings","at"}` when a poll diff qualifies for attention
  return. `cleared` lists the hide-kind marks removed
  (`settled`/`snoozed`/`archived`); it may be empty when the pane carried
  no marks (already visible — a no-op).
- `{"type":"watch-pr.tick","interval_ms","pane_filter","git_root",
  "git_root_via","targets","absent_panes","polled","deferred","failed",
  "triggered","terminal","at"}` per pass.
- `{"type":"watch-pr.stopped","reason","at"}` on exit; `reason` is
  `signal` (SIGINT/SIGTERM), `once` (`--once` with pending work),
  `no_links` (no declared PR links in scope), or `all_terminal` (every
  declared PR link reached MERGED/CLOSED or its pane is gone — matching
  the T3 "watch ends on merge/close" semantics per link).

**Trigger conditions** (diff vs the stored baseline observation only —
never PTY text or guessed state):

- `merged` / `closed` — PR lifecycle transition into MERGED/CLOSED;
  the link becomes `terminal` and is never polled again.
- `check_failed` — a check newly appears in a failing bucket
  (`fail`/`cancel` via `gh pr checks`).
- `merge_conflict` — `mergeStateStatus` transitions into `DIRTY`.
  Other `mergeStateStatus` churn (BLOCKED↔UNSTABLE↔BEHIND↔CLEAN) is
  volatile and deliberately does not wake.
- `review` — new submitted review not authored by the PR author (the
  author is the self proxy; true viewer-login detection is not
  implemented).
- `comment` — new issue or line comment not authored by the PR author.

**Attention return semantics**: a qualifying diff clears the pane's
hide-kind organize marks (`settled`/`snoozed`/`archived`) under the state
lock and re-projects metadata — the same "restore attention" direction
as `restore`, but `pin`, `order`, and declared links are preserved and
agent execution state is never touched. `blocked` and other attention
statuses were never hidden and are unaffected. Diffs are computed against
the last *successful* observation only, so a flapping or unreachable
forge cannot re-trigger: after a trigger the baseline is updated in the
same commit, and terminal links stop polling.

Baseline pruning: watch records for unlinked refs, stale links, and
closed panes are dropped (`unlink`, `pane.closed`, reconcile-time prune);
recorded baselines never resurrect for a recycled pane/agent identity.

## State and config locations

- State: `<plugin state dir>/state.json` — `HERDR_PLUGIN_STATE_DIR` under
  plugin runtime; `<state-herdr>/plugins/rebuildup.inbox/state.json` for
  external clients (`~/.local/state/herdr/...`, `$XDG_STATE_HOME` aware).
  Schema v2 namespaces entries per `hostname + canonical socket path`;
  organize state never crosses sessions or hosts.
- Config: `<plugin config dir>/config.json` (`defaultSnooze`,
  `gitPluginRoot` — explicit `rebuildup.git` plugin root override for
  `watch-pr`).
- `state.json` additionally carries an optional top-level `watch` map
  (session → pane → link ref → poll baseline/freshness record) used only
  by `watch-pr`; it is internal bookkeeping, not part of the read
  contract.
- Updates preserve both: they live outside the installed checkout.

## Versioning

`contract` is semver-ish `herdr-plugins.inbox/vN`. Additive optional fields
and new command names may appear within v1 — including new `organize.kind`
values (`archived` was added this way; consumers must tolerate unknown
kinds). Breaking changes bump the version and clients must feature-detect
via `capability`/`contract`.
