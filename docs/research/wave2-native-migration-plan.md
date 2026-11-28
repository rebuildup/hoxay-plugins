# Research: Wave 2 native-migration plan — remaining plugins (Issue #296, Program #286)

Status: investigation + decomposition input for the Wave-2 program
ticket. **No implementation is done here**; this document inventories the
real manifest surface on `release-0-13-0`, reconciles it against the
#290 disposition ledger, classifies each plugin's I/O profile, proposes
sequencing, and drafts child tickets for the coordinator to file after
review. English working record; Issue/PR prose stays Japanese per repo
convention.

Base ref: `origin/release-0-13-0` @ `41b5102`. Parent program #286;
gate #295 determines rollout, but this inventory is independent of the
gate outcome.

## 0. Executive summary

- 14 plugin manifests exist; **all 14 ship a `[[panes]]` + `open`
  action** (TUI program #228 complete) and every entrypoint is
  `bun run src/*.ts` — the Bun prerequisite is uniform and total.
- #290's `disposition-ledger.md` already classifies every plugin
  **keep**; there are **no deprecation candidates among capabilities**.
  Wave 2 is therefore a pure native-port program plus the two recorded
  distribution/runtime deprecations (Bun prerequisite, `packages/tui`
  after parity).
- Wave 1 (#291 inbox, #292 git, #293 conversations, #294 usage) owns 4
  plugins. **Wave 2 owns the remaining 10**: projects, checkpoints,
  controls, handoff, mcp, automations, artifacts, secrets, machines,
  textgen — matching the issue text exactly, no omissions.
- All shared Rust infrastructure (`crates/herdr-client`,
  `crates/tui`) lives **only on unmerged branches 291/293/294**. On
  `release-0-13-0` there is no `crates/` directory. Every Wave-2 ticket
  therefore depends on the Wave-1 merge landing first (or must vendor
  the pattern). This is the single hard sequencing constraint.
- `docs/program/tui-coverage.json` was checked: every plugin already
  records an implemented interactive TUI. **No coverage gaps found — no
  ledger update required by this ticket.**

## 1. Manifest inventory (release-0-13-0, all `bun run` argv)

| Plugin | ver | panes | actions | events/startup | CLI commands | external processes | sibling-plugin CLI calls |
| --- | --- | --- | --- | --- | --- | --- | --- |
| artifacts | 0.2.0 | main | open | — | 11 (register/list/get/search/verify/delete/deliver/gc/export + capability/help) | `open` (external preview), fs stat | none |
| automations | 0.8.0 | main | open | events ×22 (`src/hook.ts`) | 15 (create/update/delete/set-enabled/run-now/runs/schedule install\|uninstall\|status/tick/watch/fire/reconcile + capability/help) | `launchctl`, `systemctl --user`, `schtasks`; herdr socket for `agent.prompt`/`agent.start` delivery | destination `handoff-execute` reserved/not-implemented (v1) |
| checkpoints | 0.4.0 | main | open, create | events ×2 (`src/hook.ts`) | 13 (create/save/list/show/diff/plan/restore/restore-group/delete/gc/prune + capability/help) | `git` argv heavy (hidden refs, `add -A`, restore); `packages/git-core` helpers | → inbox (auto-capture drain via `auto.ts`) |
| controls | 0.6.0 | main | open | — | 9 (send/command/wait/interrupt/read/rename/start + capability/help) | herdr socket only (`agent.*`, `session.snapshot`, `pane.focus`) | none |
| conversations | 0.6.0 | main | open | — | 9 | transcript file parsing (claude/codex/opencode) | — (Wave 1 #293) |
| git | 0.7.0 | main, forge | open, open-forge, status, files (+ pane read actions) | — | 18 (status/diff/files/stage/unstage/commit/push/fetch/branch/remote/upstream/log/compare/issue/pr/project + capability/help) | `git` argv; `gh` argv for forge (GitHub only) | → inbox (PR/Issue links) (Wave 1 #292) |
| handoff | 0.7.0 | main | open | — | 10 (create/status/list/execute/delegate/merge-back/resume-info/remote-probe + capability/help) | `herdr` CLI bridge to saved machines (`--machine`-style spawn per call) | → conversations, checkpoints, controls, inbox, projects (tui-support) |
| inbox | 0.7.0 | main | open + 8 organize actions | startup:reconcile; events ×2 | 18 | `gh` (watch-pr), socket | → git (pluginroot for watch-pr) (Wave 1 #291) |
| machines | 0.1.0 | main | open | — | 4 (list/probe + capability/help) | `herdr` CLI `SavedSshApiBridge` path | none |
| mcp | 0.7.0 | main (diagnostics only) | open | **none** (stdio server, ADR-0025) | 3 (serve + capability/help) | stdio JSON-RPC; spawns sibling plugin CLIs | → conversations, git, projects, usage, controls, handoff (plugincli.ts) |
| projects | 0.4.0 | main, scripts | open, open-scripts | events ×1 (`src/hook.ts`) | 21 (register/discover/list/show/add/remove/pin/unpin/archive/unarchive/repos/settings/scripts/run/stop/results/logs/tick/update + capability/help) | shell-in-pane script runner (sh/cmd/powershell wrappers, marker-parse), `git` via sibling | → git (`plugincli.ts`), → inbox (`inbox-source.ts` transitions drain) |
| secrets | 0.2.0 | main | open | — | 6 (config/resolve/run/inject + capability/help) | `infisical` CLI per resolve (token via env, never argv/logs) | none |
| textgen | 0.8.0 | main | open | — | 6 (generate/config/git-context/watch-next-prompt + capability/help) | herdr socket agent-pane path; HTTPS `fetch` to OpenAI/Anthropic/OpenAI-compatible endpoints (opt-in) | → git, usage, conversations |
| usage | 0.6.0 | main | open | — | 6 (usage/quota/cost/divergence + capability/help) | HTTPS `fetch` to provider quota endpoints (claude/codex) | → conversations (index reads) (Wave 1 #294) |

Manifest cross-check vs #290 ledger: **no drift**. Every plugin is
`keep`; surfaces, contract ids, and the two upstream-delegate records
(artifacts render→herdr-t3, automations webhook→ADR-0026,
controls dialogs→herdr#6196) match the ledger exactly.

## 2. Shared-code dependency on Wave 1 (hard constraint)

The Wave-1 port pattern (evidence: `docs/research/inbox-native-rust.md`
on branch `291`, `conversations-native-rust.md` on `293`, and the #294
PR) establishes:

- `crates/herdr-client` — NDJSON socket RPC, session/socket
  resolution, atomic state-file helpers.
- `crates/tui` — `safety.rs` (`sanitize_display`/`bound_text`/
  `BoundedBuffer`) now; the ratatui/crossterm app shell is deferred
  "until a second consumer" — **Wave 2 is that second consumer**, so
  extracting the shared TUI shell is itself Wave-2 work.
- Per-plugin committed binary `plugins/<name>/bin/<name>` + manifest
  argv `./bin/<name> ...` (ADR-0035), honest `platforms` narrowing.
- TS↔Rust parity harness `scripts/diff-<plugin>.ts` (fixture-driven,
  `git archive` of the pinned pre-removal commit) — the acceptance
  pattern every Wave-2 ticket must replicate.
- Sibling-call pattern: callee plugins resolve `bin/<name>` first,
  `bun src/cli.ts` fallback (ADR-0035 transitional shim, removed in
  Final #298).

**Consequence:** no Wave-2 plugin ticket can land before the Wave-1
gate (#295) merges at least the shared crates. Tickets may still do
*investigation/design* in parallel (per #296), but implementation is
gated. Proposed child issues below are ordered accordingly.

## 3. Per-plugin analysis

I/O classification legend: **compute** = port fully to Rust (no
meaningful external process boundary); **I/O-bound** = external CLI/HTTP
is the sanctioned boundary — keep the call, port orchestration/state/TUI
and evaluate call-reduction/maintainability (program §"外部I/O律速").

### 3.1 rebuildup.controls — compute, thin, lowest risk

- Surface: 9 commands, all thin `agent.*`/`session.snapshot`/`pane.focus`
  socket wrappers with `--expect-*` stale-generation guards.
- I/O: herdr socket only. No external CLI, no state file of its own
  beyond config.
- Reuse: `crates/herdr-client` covers ~all of it; TUI is a
  list/detail/form surface — ideal first consumer of the shared TUI
  shell extraction.
- Risk: low. Structured dialogs stay `unsupported` (herdr#6196) — do
  not regress the honest-unsupported record.
- Maps to existing issues: #27 (Provider Controls umbrella — closed
  scope, this is native port not feature work). No new feature issue
  needed.

### 3.2 rebuildup.artifacts — compute + local FS, low risk

- Surface: 11 commands over a JSON registry + file metadata; `deliver`
  emits references; `open` preview shells the OS `open`.
- I/O: filesystem stat/read for validation; OS open is a trivial spawn.
  No network, no sibling calls.
- Reuse: state-file helpers from herdr-client; TUI = browser/detail/
  confirm forms.
- Risk: low. Render/screenshot/MCP-Apps stay delegated to herdr-t3
  (ADR-0024) — port must not grow that surface.
- Maps to: #31 (Artifacts umbrella, closed scope). Port only.

### 3.3 rebuildup.machines — I/O-bound thin, lowest priority but trivial

- Surface: 4 commands; `probe` spawns the `herdr` CLI SavedSshApiBridge
  path per machine.
- I/O: every read is an external `herdr` CLI invocation over SSH
  bridge — genuinely I/O-bound; the plugin is a thin honest-inventory
  boundary (ADR-0028). Port value is mostly Bun-removal, not speed.
- Reuse: minimal; TUI is read-only list + probe progress/cancel.
- Risk: low code volume, but **Windows/SSH-matrix verification cost is
  the real risk** — schedule late, keep `platforms` honest.
- Maps to: #34-adjacent machines work (ledger), no open feature issue.
  Candidate for bundling with another small plugin if the coordinator
  wants fewer tickets — flagged as open question §8.

### 3.4 rebuildup.secrets — I/O-bound provider boundary, security-critical

- Surface: 6 commands; `resolve`/`run`/`inject` spawn `infisical`
  secrets-get per invocation (token via env only, JSON stdout).
- I/O: `infisical` CLI is the provider boundary — **keep the spawn**;
  the ADR-0027 invariant (values never persisted/logged/argv/emitted)
  must be re-proven in Rust, including stderr sanitization.
- Reuse: state/config helpers; TUI = bindings browser, metadata-only
  (values never rendered).
- Risk: medium — security boundary + open finding **#209** (provider
  stderr embedded in error detail needs sanitize) must be fixed or
  explicitly carried into the port acceptance.
- Dependency-removal evaluation (program requirement): Infisical is an
  external product boundary, not a removable dependency; the eval
  outcome should be recorded (keep CLI; alternative = Infisical API
  direct is a contract-level change → maintainer judgment, not assumed).
- Maps to: secrets inventory research exists
  (`secrets-remote-devices-inventory.md`); #209 is the only open defect.

### 3.5 rebuildup.checkpoints — git-CLI-bound, medium

- Surface: 13 commands; hidden-ref capture/save/restore/groups/gc over
  `git` argv + `packages/git-core` env/path helpers; auto-capture via
  hook marks drained lazily.
- I/O: git subprocess is the sanctioned boundary (program §Git:
  mutations stay argv; gix read-only only with proven equivalence).
  Checkpoint ops are git-*mutation*-class → keep `git` argv entirely;
  the port is orchestration/state/hook/TUI.
- Reuse: `packages/git-core` logic must be re-expressed in Rust (or a
  shared `crates/git-adapter` if git plugin #292 establishes one —
  coordinate, don't duplicate).
- Risk: medium — restore safety guards and pending-mark drain are the
  subtle parts; `pane.closed` mark-drop semantics must parity-test.
- Maps to: #4 umbrella (closed). No open feature issue.

### 3.6 rebuildup.automations — OS-scheduler I/O + hooks, medium-high

- Surface: 15 commands + **22 `[[events]]` hook kinds** + `watch`
  long-running process + `schedule install|uninstall|status` driving
  `launchctl`/`systemctl --user`/`schtasks` adapters.
- I/O: mixed — OS scheduler CLIs stay (they ARE the mechanism, no
  daemon allowed per ADR-0002); firing delivers via herdr socket
  `agent.prompt`/`agent.start`; event marks drain lazily on `tick`.
- Reuse: herdr-client for delivery; hook entry is a separate argv
  (`src/hook.ts`) → becomes `bin/automations hook`.
- Risk: medium-high — largest event surface, three OS scheduler
  adapters, misfire policies, idempotent `task_id+scheduled_for` runs.
  Windows schtasks path is the least-verified leg.
- Maps to: #32 umbrella (closed); webhook stays `unsupported_schedule`
  (ADR-0026) — do not regress.
- Call-reduction eval: `tick`/`watch` spawn cadence is user-configured;
  no ambient polling exists to remove — record that finding.

### 3.7 rebuildup.handoff — sibling-CLI orchestration, medium-high

- Surface: 10 commands; records bind pane/agent generation to reference
  payloads (conversations/checkpoints excerpts); `execute`/`delegate`/
  `merge-back`/`resume-info`; `remote-probe`/`remote` via `herdr` CLI
  bridge to saved machines.
- I/O: local state + **five sibling-plugin CLI calls** (conversations,
  checkpoints, controls, inbox, projects) + remote `herdr` CLI spawns.
  The sibling boundary stays (ADR-0013); what changes is the callee
  binary (`bin/<name>` first, bun fallback).
- Reuse: herdr-client; TUI is the richest form set (target pickers,
  confirm gates, return-to-origin focus).
- Risk: medium-high — lineage/resume correctness across generations and
  remote machines; depends on conversations+checkpoints+controls being
  native for the *efficient* path but works via fallback earlier.
- Maps to: #6 umbrella (closed), #196 (side-chat fork lineage eval —
  conditional follow-up, not blocking).

### 3.8 rebuildup.textgen — socket orchestration + HTTPS, medium-high

- Surface: 6 commands; six generation tasks; default path drives a
  dedicated herdr agent pane via socket (`workspace.create` →
  `agent.start` → `agent.prompt --wait` → `agent.read` → cleanup);
  opt-in API path is HTTPS `fetch` (OpenAI/Anthropic/OpenAI-compat,
  https-only except loopback — ADR-0018).
- I/O: agent-pane path is socket-bound (compute orchestration);
  API path is network I/O — keep an HTTP client in Rust (reqwest or
  minimal), the endpoint/key boundary is unchanged.
- Reuse: herdr-client heavily; reads git-context via sibling git +
  conversations, quota via sibling usage.
- Risk: medium-high — `watch-next-prompt` is a long-lived subscription
  loop; prompt assembly/output validation must parity-test byte-exact;
  credential boundary (ADR-0018) re-proven.
- Maps to: #29 umbrella (closed), **#249** (result retention contract
  gap — maintainer decision, NOT a port blocker; port preserves
  persist-nothing behavior).

### 3.9 rebuildup.mcp — protocol server + fan-out, high complexity

- Surface: `serve` = stdio MCP JSON-RPC server (no hooks, per-client
  spawn, ADR-0025) + diagnostics TUI (capability/access/session/tools/
  per-plugin availability).
- I/O: compute-bound protocol handling; **every tool call fans out to
  sibling plugin CLIs** via `plugincli.ts` (env-pinned
  `HERDR_PLUGIN_*`/`--socket`). The process boundary is the contract —
  preserved; the win is removing Bun from the per-client spawn path and
  reducing spawn overhead if siblings are native.
- Reuse: herdr-client for session/socket; an MCP SDK decision is
  explicitly **Wave 3 (#297)** scope — Wave 2 ports the existing
  hand-rolled protocol surface as-is; do not adopt an SDK here.
- Risk: high — largest single surface (`tools.ts`, `events.ts`
  subscriptions, session resolution); conformance is Wave-3's problem
  but the port must not change wire behavior.
- Maps to: #30 umbrella (closed); #297 owns optimization/contract.

### 3.10 rebuildup.projects — largest surface, highest risk, last

- Surface: 21 commands, **two TUIs** (registry + scripts), `worktree.
  created` hook marks, `tick` lazy drain, runOnSettle/stopOnSettle via
  inbox transitions, repos↔worktree/diff association via sibling git,
  and the **scripts runner**: injects shell-specific wrapper commands
  into a dedicated pane, detects shell family (sh/cmd/powershell +
  unsupported-shell honest failures), parses marker exit codes from
  scrollback.
- I/O: mixed — socket for pane injection/reads, sibling git CLI for
  association, sibling inbox CLI for transitions drain, local state.
- Reuse: everything — herdr-client, TUI shell (two surfaces), git
  adapter boundary.
- Risk: highest — the runner's shell-family detection and scrollback
  marker parsing are the subtlest code in the repo; cross-platform
  script execution was itself a dedicated guarantee (#65). Needs the
  most parity fixtures.
- Maps to: #26 umbrella (closed), #60/#182 (triggers — implemented via
  inbox transitions; verify parity, no new feature work), #65 (scripts
  guarantee — acceptance reference).

## 4. Cross-plugin dependency graph (sibling CLI edges, caller → callee)

```
inbox ──→ git                      (watch-pr pluginroot, #178)
git ────→ inbox                    (PR/Issue links)
projects→ git, inbox               (association reads, transitions drain)
checkpoints→ inbox                 (auto-capture drain)
handoff → conversations, checkpoints, controls, inbox, projects
mcp     → conversations, git, projects, usage, controls, handoff
usage   → conversations            (Wave 1 both)
textgen → git, usage, conversations
automations → handoff              (reserved destination, not implemented v1)
```

Wave-2-native ordering must respect: handoff/textgen/mcp/projects all
*call* Wave-1 or Wave-2 plugins — they can start earlier (bun-fallback
works) but reach full efficiency only after callees are native.

## 5. Proposed sequencing

Constraint: shared crates land with Wave-1 merge. Within that:

| tranche | plugins | rationale |
| --- | --- | --- |
| W2-A (thin proofs) | controls, artifacts, machines | smallest surfaces, no/few sibling edges; controls+artifacts prove the shared TUI-shell extraction; machines is trivial code but defer OS-matrix verification |
| W2-B (I/O-bound evals) | secrets, checkpoints, automations | each exercises a different "keep the external call" boundary (infisical CLI, git argv, OS schedulers); independent of each other |
| W2-C (orchestrators) | handoff, textgen | heavy sibling fan-out; start after W2-B core callees native; textgen API-path HTTP client is self-contained |
| W2-D (hub) | mcp | benefits from ALL siblings native (spawn `bin/*` not `bun`); largest protocol surface; keep wire behavior frozen |
| W2-E (largest) | projects | needs git+inbox (Wave 1) native and benefits from everything; scripts runner gets the deepest parity harness; two TUIs exercise the shared shell hardest |

Each plugin still follows the program pipeline: contract freeze → Rust
impl → diff-verified parity → manifest argv switch → rollback confirm
→ old TS removal. Tranches are ordering guidance, not a barrier —
independent tickets may run in parallel once crates exist.

## 6. Proposed child issues (drafts — coordinator files after review)

No existing open issue duplicates these; each references its umbrella
(#25–#33 feature issues are closed-scope parents, not port tickets).
Titles English; bodies Japanese at filing time.

| # | Title (draft) | Scope | Acceptance draft |
| --- | --- | --- | --- |
| C1 | Controls native port (#296 child) | cli/hook n/a; `bin/controls` CLI+TUI; socket-only domain | diff harness parity vs pinned TS commit; tui-v1 contract tests; `platforms` honest; bun argv gone from manifest |
| C2 | Artifacts native port | registry domain + `open` preview spawn + TUI | same parity pattern; no render/capture surface added (ADR-0024) |
| C3 | Machines native port | list/probe + herdr CLI bridge + TUI | parity; SSH/bridge behavior unchanged; verification matrix recorded per-OS |
| C4 | Secrets native port + #209 sanitize | infisical spawn boundary, env-only token, run/inject, TUI | ADR-0027 re-proven (no value in argv/logs/state); #209 stderr-sanitize fixed or explicitly deferred with maintainer note |
| C5 | Checkpoints native port | hidden-ref git orchestration + hook marks + drain + TUI; git stays argv | restore/conflict/mark-drop parity fixtures; `packages/git-core` logic disposition decided (shared crate vs local) |
| C6 | Automations native port | 22 event hooks, scheduler adapters (launchctl/systemctl/schtasks), tick/watch, delivery via socket, TUI | per-OS scheduler adapter parity; webhook stays unsupported_schedule; misfire/idempotency fixtures |
| C7 | Handoff native port | records/lineage/refs + execute/delegate/merge-back/resume-info/remote-probe + TUI | sibling bin-or-bun resolution for all 5 callees; remote herdr CLI bridge parity; generation-guard fixtures |
| C8 | TextGen native port | 6 tasks, agent-pane path, HTTPS API path, git-context/quota sibling reads, watch-next-prompt, TUI | ADR-0018 endpoint/key boundary re-proven; prompt/output validation byte-parity; #249 explicitly out of scope |
| C9 | MCP native port | `serve` stdio JSON-RPC + event subscriptions + diagnostics TUI | wire-protocol parity (tools/list, calls, events_*); sibling fan-out via plugincli resolution; **no SDK adoption** (Wave 3) |
| C10 | Projects+Scripts native port | registry/repos/settings/scripts runner/tick/triggers + 2 TUIs + hook | shell-family detection + marker-parse parity matrix (sh/cmd/powershell/unsupported); #65 guarantees re-verified; largest diff harness |
| C11 | Shared TUI shell extraction | promote ratatui/crossterm app shell into `crates/tui` once ≥2 consumers exist (controls+artifacts in W2-A) | ADR-0036 boundary respected; tui-v1 contract tests; used by subsequent ports |
| C12 | Sibling-caller sweep | update every `plugincli.ts`-style resolver to prefer `bin/<name>` (handoff, mcp, projects, textgen, usage→done in W1, automations reserved) | fallback retained until Final; covered per-plugin in C1–C10 where the caller lives — **may fold into those tickets**; keep only if coordinator prefers a separate sweep |

Mapping check against the do-not-duplicate list: #263 (contract
metadata — additive, applies per-plugin opportunistically, not a port
ticket), #249/#209/#306/#313/#314 (pre-existing defects/gaps carried
into relevant tickets' acceptance), #315/#316 (cross-cutting
distribution — own tickets), #191–#193 (forge providers — git plugin,
Wave 1), #178 (inbox watcher — Wave 1), #182/#60/#65 (feature guarantees
used as acceptance references), #30–#33/#25–#29/#6–#9 (closed or
upstream-gap umbrellas). **No omissions vs the issue's plugin list; no
duplicates.**

## 7. External-I/O-bound evaluation summary (program requirement)

| plugin | external boundary | verdict |
| --- | --- | --- |
| secrets | `infisical` CLI | keep spawn; API-direct is contract change → maintainer judgment only |
| checkpoints | `git` argv | keep argv entirely (mutations); no gix here |
| automations | launchctl/systemctl/schtasks | keep — they are the no-daemon mechanism; no call reduction applicable (no ambient polling exists) |
| machines | `herdr` CLI bridge | keep; thin boundary is the design (ADR-0028) |
| handoff | `herdr` CLI remote bridge | keep; per-call process is upstream bridge lifetime |
| textgen | HTTPS API (opt-in) | keep HTTP; agent-pane default path is socket-native |
| usage (W1) | provider quota HTTPS | already ported in #294; #314 tracks residual spawn cost |
| git (W1) | `git`/`gh` argv | already decided in #292 |

For every I/O-bound plugin the port still removes Bun from the hot path
(spawn `bin/*` directly), which is the maintainability/call-reduction
win the program asks to evaluate — recorded per ticket, not assumed.

## 8. Open questions for the maintainer / coordinator

1. **Tranche vs. per-plugin tickets**: 10 plugins → 10 tickets (C1–C10)
   plus shared-shell (C11) is the clean mapping, but machines+artifacts
   or secrets+checkpoints could merge if review bandwidth is tight.
   Recommend keeping per-plugin for independent rollback (program
   delivery model).
2. **C12 sibling sweep**: fold into each caller's port ticket (my
   recommendation — the resolver change is small and belongs with the
   plugin's own diff) or a dedicated sweep ticket?
3. **`crates/tui` shell timing**: extract after W2-A's first two
   consumers (controls+artifacts) per ADR-0036's stated trigger, or
   earlier/later? Affects C11 scheduling.
4. **machines OS matrix**: probe path is SSH-bridge-dependent; Linux/
   Windows verification may need dedicated real-machine sessions —
   acceptable to land macOS-verified with honest `platforms`, matching
   Wave-1 precedent?
5. **multicall reconsideration** (ADR-0035 §3 deferred): with 10 more
   binaries, does the coordinator want a size/coexistence re-evaluation
   before W2-D, or stay per-plugin binaries?
6. **#249/#209 disposition**: carry as explicit non-goals in C8/C4
   acceptance (recommended) or file separate tickets now?

## 9. TUI coverage check

`docs/program/tui-coverage.json` already records an implemented
interactive TUI for all 14 plugins (verified against manifests: every
plugin has `[[panes]]` + `open`-family actions). No new human-facing
feature is introduced by Wave 2 — it is a runtime/distribution change —
so **no ledger update is required**. The one genuine TUI-program gap
this analysis surfaced is internal, not contractual: the shared
ratatui/crossterm shell (`crates/tui`) does not exist yet on any merged
branch — tracked as proposed issue C11.
