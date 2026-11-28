# Research: manifest/capability disposition ledger and minimal #263 contract mapping (Issue #290, Program #286)

Status: decision-input research for the Native Performance program
(#286). Non-blocking inventory for Wave 1+ and Wave 3. Records
disposition *candidates* with rationale; **no capability is removed
here**, and every breaking change is listed for explicit maintainer
judgment (§7). English working record; Issue/PR prose stays Japanese
per repo convention.

Disposition legend (program #286):

- **keep** — capability and contract stay; internals may be rewritten
  (e.g. Bun→Rust) without contract change.
- **redesign** — capability stays but the mechanism must change
  (e.g. self-CLI respawn removed, index rebuilt incrementally).
- **integrate** — merge into a shared mechanism/package or sibling
  plugin's public contract rather than a standalone surface.
- **upstream-delegate** — belongs to herdr core / herdr-t3 / an
  external tool; plugin keeps only an honest `unsupported`/gap record.
- **deprecate-candidate** — removal candidate pending maintainer
  judgment + migration conditions (§7). Never removed without it.

`feature-ledger.json` was inspected first: its schema
(`schema_version: "1.0.0"`, fixed `classification_legend`, per-feature
fields `id/title_ja/upstream_refs/classification/issues/depends_on/
acceptance/verification`) has **no disposition field and no
per-capability granularity**. Adding fields would be a schema change
outside this ticket's authority, so dispositions live in this document;
the ledger's existing `classification`/`verification` fields remain
the feature-level status of record.

## 1. Public contract ledger

All contracts live in `docs/contracts/`; versioning policy is in
`docs/contracts/README.md` (additive extends v1, breaking → `/v2`).

| Contract | Doc | Status | Primary consumers | Disposition |
| --- | --- | --- | --- | --- |
| `herdr-plugins.inbox/v1` | inbox-v1.md | stable (additive v0.4.0–v0.7.0) | herdr-t3, projects (`transitions` drain), MCP | keep; additive `command_metadata` per #263 is the only planned extension |
| `herdr-plugins.git/v1` | git-v1.md | stable | textgen, projects (read commands), MCP, TUI | keep; read internals may adopt gix only after equivalence proof (program §Git) |
| `herdr-plugins.projects/v1` | projects-v1.md | stable (additive v0.4.0) | TUI, MCP | keep |
| `herdr-plugins.checkpoints/v1` | checkpoints-v1.md | stable | handoff refs, MCP, TUI | keep |
| `herdr-plugins.conversations/v1` | conversations-v1.md | stable | usage, handoff, textgen, MCP, TUI | keep; index internals → incremental/SQLite-FTS5 evaluation (program §Conversations) |
| `herdr-plugins.usage/v1` | usage-v1.md | stable | MCP, TUI | keep |
| `herdr-plugins.controls/v1` | controls-v1.md | stable | MCP (stale guards, permission cap), TUI | keep; structured dialogs stay `unsupported` pending herdr#6196 (ADR-0019) |
| `herdr-plugins.handoff/v1` | handoff-v1.md | stable | MCP, TUI, automations (`handoff_record_id`) | keep |
| `herdr-plugins.textgen/v1` | textgen-v1.md | stable | git/forge callers (generation only), MCP, TUI | keep |
| `herdr-plugins.artifacts/v1` | artifacts-v1.md | stable | MCP, TUI | keep (registry only) |
| `herdr-plugins.automations/v1` | automations-v1.md | stable | OS schedulers + hooks, TUI | keep; webhook stays `unsupported_schedule` (ADR-0026) |
| `herdr-plugins.secrets/v1` | secrets-v1.md | stable | run/inject callers, TUI | keep; Infisical-only v1 unchanged |
| `herdr-plugins.machines/v1` | machines-v1.md | draft→stable surface, read-only | TUI, diagnostics | keep; thin inventory, low rewrite priority |
| `herdr-plugins.mcp/v1` | mcp-v1.md | stable | outside MCP clients | keep; Wave 3 owns optimization/conformance (see acp-mcp-interop-evaluation.md §4) |
| `herdr-plugins.tui/v1` | tui-v1.md | stable (ADRs 0031–0033) | all 14 plugin TUIs | keep; contract survives the Rust rewrite — presentation adapter only |

## 2. Per-plugin manifest / capability dispositions

Manifests inventoried: all 14 `plugins/*/herdr-plugin.toml` at
release-0-13-0 base. Every plugin currently ships `[[panes]]` +
`open`-family `[[actions]]` (TUI program #228 complete); hooks are
`[[startup]]`/`[[events]]` per ADR-0002/0005 (no resident processes).

| Plugin (id) | Manifest surfaces | Capability groups | Disposition | Rationale |
| --- | --- | --- | --- | --- |
| rebuildup.inbox (0.7.0) | panes:main; actions: open + 8 pane-context organize actions; startup:reconcile; events: agent_status_changed/closed | organize state (settle/snooze/restore/pin/order/archive), links, transitions log, watch-pr | **keep** (redesign internals → Rust, Wave 1 #291) | Core user value; contract stable; `transitions` already the invalidation feed other plugins consume |
| rebuildup.git (0.7.0) | panes:main+forge; actions: open/open-forge + 5 pane read actions | git-v1 read/write/forge (gh CLI) | **keep**; git read path **integrate/evaluate gix** only with proven equivalence (program §Git); gh CLI delegation stays | Mutations already argv-spawn gh/git; gix read-only is a Wave-2 candidate, not a given |
| rebuildup.projects (0.4.0) | panes:main+scripts; actions: open/open-scripts; events:worktree.created | registry, repos, settings, scripts runs, triggers | **keep** (redesign Wave 2 #296) | Heavy lazy-drain/event-hook design already matches "no daemon" invariant |
| rebuildup.checkpoints (0.4.0) | panes:main; actions: open+create; events ×2 | hidden-ref capture/restore/groups/gc | **keep** | Hidden refs are the correct herdr-native mechanism; auto-capture via hook marks only |
| rebuildup.conversations (0.6.0) | panes:main; action:open | transcript indexer (claude/codex/opencode) | **keep**; index **redesign** (incremental, no full re-parse; SQLite/FTS5 by measurement) | Upstream gap #6191 closed NOT_PLANNED — path-kind resume stays unavailable; do not fabricate |
| rebuildup.usage (0.6.0) | panes:main; action:open | token/quota/cost aggregation over conversations index | **keep** | Read-only recompute; benefits directly from conversations index redesign |
| rebuildup.controls (0.6.0) | panes:main; action:open | agent ops (send/wait/interrupt/read/rename/start+launch args) | **keep**; structured dialogs **upstream-delegate** (herdr#6196) | agent.* 1:1 thin wrapper is the correct boundary; dialogs honestly `unsupported` |
| rebuildup.handoff (0.7.0) | panes:main; action:open | records/delegate/execute/remote-probe | **keep** | References-not-copies payload boundary already correct |
| rebuildup.textgen (0.8.0) | panes:main; action:open | 6 generation tasks, agent-pane + opt-in API paths | **keep** | Generation-only separation from execution already enforced (ADR-0017/0018) |
| rebuildup.artifacts (0.2.0) | panes:main; action:open | registry validate/index/deliver/gc/export | **keep** (registry); render/screenshot/MCP-Apps **upstream-delegate** to herdr-t3 (ADR-0024) | Plugin correctly refuses OS capture/browser preview |
| rebuildup.automations (0.8.0) | panes:main; action:open; events ×22 | schedules, OS-scheduler fire, event hooks, runs | **keep**; webhook **upstream-delegate** (ADR-0026, `unsupported_schedule`) | OS scheduler + lazy tick drain is the sanctioned no-daemon design |
| rebuildup.secrets (0.2.0) | panes:main; action:open | Infisical resolve/run/inject bindings | **keep** | ADR-0027 boundary (values never persisted/logged/argv) is a security invariant |
| rebuildup.machines (0.1.0) | panes:main; action:open | read-only machine list/probe | **keep** (lowest rewrite priority) | Lifecycle stays with herdr CLI (ADR-0028); thin surface |
| rebuildup.mcp (0.7.0) | panes:main (diagnostics only); action:open; **no hooks** | stdio MCP server + diagnostics TUI | **keep**; Wave 3 **integrate** into standard-conformance work | Boundary settled (ADR-0025); see acp-mcp-interop-evaluation.md §4 |

### Shared packages

| Package | Role | Disposition | Rationale |
| --- | --- | --- | --- |
| `packages/herdr-client` | typed herdr socket client | **keep** → becomes the protocol-adapter layer in the Rust workspace | Single authority path to herdr API; Rust port preserves the same request/response contract |
| `packages/git-core` | git env/path helpers | **keep**; evaluate merge into Rust git adapter or gix boundary | Shared by git plugin (+consumers via CLI, not imports) |
| `packages/tui` | zero-dep ANSI/TTY renderer (ADR-0032) | **keep** until Rust TUI lands, then **deprecate-candidate** (§7.4) | Contract `tui-v1` is renderer-agnostic; TS renderer is the reference implementation for parity testing |

### Cross-cutting pattern dispositions

| Pattern | Where | Disposition | Note |
| --- | --- | --- | --- |
| TUI → own-plugin CLI respawn | every plugin's `src/tui.ts` shells `src/cli.ts` | **redesign** | Program #286: same-plugin surfaces call the domain directly; process/IPC boundary remains only for *independently installed* plugins (ADR-0013 sibling-CLI pattern stays for cross-plugin calls, e.g. git→inbox links, projects→inbox transitions, mcp→siblings) |
| Bun runtime prerequisite | all manifests (`bun run …`) | **deprecate-candidate** for distribution | ADR-0003 superseded by the Rust program; migration condition §7.1 |
| `mutation_commands` capability field | all CLI plugins (#224) | **keep** + extend via #263 `command_metadata` (additive) | Consumers already treat it as conservative superset |
| Lazy-drain event marks (`[[events]]` hook → next CLI call) | inbox, checkpoints, projects, automations | **keep** | Matches ADR-0002 no-daemon invariant; survives Rust rewrite as hook+drain |
| Plugin→sibling-plugin CLI spawn (ADR-0013 env-pin) | git→inbox, projects→inbox, mcp→siblings, textgen→git, usage→conversations | **keep** | Required process boundary for independent install/update/disable; program explicitly preserves it |

## 3. Upstream-delegate register (already filed — do not duplicate)

| Gap | Upstream | Record |
| --- | --- | --- |
| Structured dialogs / approval-question API | herdrdev/herdr#6196 | ADR-0019; controls `capability.dialogs` = unsupported |
| Message admission / queue-steer | herdrdev/herdr#6197 | ADR-0019/#101 verdict |
| Remote MCP HTTP/OAuth ingress | herdrdev/herdr#6199 | ADR-0025; local stdio only |
| `agent_session_path` exposure | herdrdev/herdr#6191 | closed NOT_PLANNED — path-kind resume unavailable by upstream decision |
| Webhook triggers | ADR-0026 | automations `unsupported_schedule` |
| agent.view single slot | #9 (ledger) | ADR-0004; TUI panes are the working surface |
| Attention granularity | #8 (ledger) | `blocked` aggregation is upstream truth; never hidden |
| OS capture / browser preview / MCP Apps render | herdr-t3 (#31/ADR-0024) | artifacts contract boundary |
| SnapShots, guest links, T3 Connect, managed policy | secrets-remote-devices-inventory.md | out-of-scope/upstream-blocked records |

## 4. Deprecation candidates (explicit — nothing removed here)

Only two genuine candidates emerged; both are *delivery/runtime*
concerns, not capability removals:

1. **Bun/Node runtime prerequisite** (all plugins) — deprecate-candidate
   contingent on §7.1.
2. **`packages/tui` TypeScript renderer** — deprecate-candidate once
   the Rust TUI reaches feature parity (§7.4).

No plugin capability, contract command, or manifest surface is a
deprecation candidate. `rebuildup.machines` is thin but is the
sanctioned read-only inventory boundary (ADR-0028) — keep.

## 5. Minimal #263 contract subset for Inbox (#291)

#263 proposes per-command metadata (`kind`, `idempotent`, `coalesce`,
`freshness`, `invalidation`) so consumers (herdr-t3 #416/#424 data
plane; future hoxay clients) can cache/retry/coalesce safely. **Reuse
#263 as the contract-SoT work — do not create duplicate tickets, do not
close #263.** Inbox (#291) needs only this minimal subset:

| #263 field | Inbox commands needing it | Minimal value for #291 | Existing mechanism it binds to |
| --- | --- | --- | --- |
| `kind` | all | `read`: `list`, `links`, `transitions`, `capability`, `help`; `mutation`: the 13 `mutation_commands` entries | already split via `commands`/`mutation_commands` (#224); #263 object-ifies it |
| `idempotent` | mutations | `settle`/`pin`/`unpin`/`archive`/`unarchive`/`link`/`unlink`/`order`/`restore`/`reconcile` = **true** (same args → same state; generation guards already reject stale targets); `snooze` = true per same deadline args (re-snooze with new args is a new intent, not a retry hazard); `watch`/`watch-pr` = n/a (daemon-class, excluded from retry semantics) | settle/snooze already generation-guarded (agent_session fingerprint + `state_change_seq`); contract documents stale refusal |
| `coalesce` | reads | `list`, `links`, `transitions` = **true** (pure reads over plugin state + socket snapshot; single-flight safe); mutations = false | none today — this is the new information #263 adds |
| `freshness` | `list`, `links`, `transitions` | `event-driven` for `list`/`links` (state changes only via inbox mutations or herdr lifecycle events); `snapshot-consistent`/`cursor` for `transitions` (seq-cursor pagination is already the freshness contract — `--after <seq>`, `cursor_expired` on cap eviction) | `transitions` log (ADR-0029, inbox v0.4.0) is already the invalidation feed projects consumes via lazy drain |
| `invalidation` | `list`, `links`, `transitions` | `pull` via `transitions --after`; push via herdr `pane.agent_status_changed`/`pane.closed` events (already `[[events]]` hooks); no plugin push channel in v1 | ADR-0029 transitions + manifest event hooks |
| target identity | all pane-scoped commands | already carried: Host/Session/Workspace/Pane + `agent_session` fingerprint + `state_change_seq`; #263 should reference, not redefine, this | inbox-v1 "Target selection" + stale-generation rules |

**How it connects:** #263 lands `command_metadata` as an *additive*
capability-document section (per its proposal, `commands`/`mutation_commands`
stay for backward compat). Inbox advertises metadata only for the
commands above; other plugins add theirs incrementally in their own
tickets — no big-bang migration. Consumers read metadata opportunistically
(absent ⇒ conservative defaults: kind from `mutation_commands`,
coalesce only for known-safe reads — matching herdr-t3's current
single-flight rule).

**Explicit later expansion (not #291 scope):** per-command metadata
for git/projects/conversations/usage reads (coalesce/freshness matter
most for the expensive index reads), MCP tool-table equivalence notes,
and consumer-side TTL-cache adoption in herdr-t3. These are #263's own
delivery tickets, not duplicates created here.

## 6. Breaking-change judgment list (maintainer decisions required)

Nothing below is executed by #290; each needs explicit maintainer
judgment plus the stated migration condition.

| # | Candidate change | Contract impact | Migration condition |
| --- | --- | --- | --- |
| 7.1 | Bun→Rust distribution per plugin (program core) | Runtime prerequisite changes (`bun run` → native binary/multicall); user MCP client configs and `[[panes]]`/`[[actions]]` commands reference argv — manifest commands must be updated atomically with the binary | Per-plugin: contract frozen → Rust impl → diff-verified parity → manifest argv switch → rollback path confirmed → old impl removed (program delivery model). `min_herdr_version`/platforms re-verified per OS |
| 7.2 | MCP `serve` argv change (`bun run src/cli.ts serve` → binary) | Users' MCP client configs embed the argv; breaking for existing setups | Ship a stable `serve` entrypoint name; document config migration; consider a thin `bun`-compat shim only if maintainer judges the transition cost worth it — otherwise a documented one-time config update per install |
| 7.3 | `command_metadata` addition to capability documents | Additive — **not breaking**; recorded here for completeness | None beyond #263's own rollout |
| 7.4 | Retire `packages/tui` TS renderer after Rust TUI parity | Internal package, not a public contract; but it is the executable reference for `tui-v1` behaviors (CJK/IME/resize/safety) | Rust renderer passes the same tui-v1 contract tests + real-machine matrix (ADR-0033) on all target OSes; keep TS impl until parity is proven, then remove in the same PR that switches manifests |
| 7.5 | Self-CLI-respawn removal inside each plugin's TUI | Internal only; public CLI/MCP/GUI behavior must stay identical (ADR-0033 non-interactive guarantee) | TUI calls domain directly; every mutation still gated by `capability`/`mutation_commands` + `--expect-*` guards; contract tests unchanged and green |
| 7.6 | gix adoption for git read paths | Internal; `git-v1` output bytes must be identical on the fixed fixture set | Equivalence proof on porcelain/-z/rename/binary/conflict edge cases + speed measurement vs argv git (program §Git: read-only, fallback retained) |

## 7. What #291 / Wave 3 can rely on

- **#291 (Inbox vertical slice):** contract work is limited to #263's
  minimal metadata subset (§5) — additive, no new ticket needed;
  `transitions` is already the invalidation mechanism; no ACP/MCP
  surface is required.
- **Wave 3 (#297):** MCP boundary/semantics are settled and preserved
  (ADR-0025 + acp-mcp-interop-evaluation.md §4); ACP is a recorded
  rejection with a defined verification path should the hypothesis be
  overturned; standard-version pins (MCP already negotiated; ACP pinned
  only at adoption) are the remaining inputs.
- **All waves:** the disposition tables (§1–§2) are the inventory of
  record for the program's "all 14 manifests evaluated" completion
  condition; updates belong in this document's successors or the
  feature ledger as tickets land.
