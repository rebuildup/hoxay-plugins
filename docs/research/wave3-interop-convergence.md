# Research: Wave 3 interop convergence — ACP decisions, MCP conformance, #263 remainder (Issue #297, Program #286)

Status: decision-convergence record for Wave 3 (#297). This document
deepens the #290 evaluation
(`docs/research/acp-mcp-interop-evaluation.md`), records the #263
`command_metadata` post-Wave-1 status, and produces the MCP
conformance gap list, sequencing vs Wave 2 (#296), and proposed child
tickets. It implements nothing and closes nothing; every adoption
item below is a proposed Issue for the coordinator to file.

Inputs (canonical):

- `docs/research/acp-mcp-interop-evaluation.md` (#290) — ACP
  adopt/reject hypothesis + per-scenario ownership table.
- `docs/research/disposition-ledger.md` §5 (#290) — minimal #263
  `command_metadata` subset for Inbox.
- GitHub Issue #263 — per-command metadata proposal (contract SoT).
- GitHub Issue #296 — Wave 2 native migration (owns `rebuildup.mcp`'s
  Rust port; this document must not duplicate it).
- `docs/contracts/mcp-v1.md`, `docs/contracts/README.md`,
  `docs/specs/mcp.md`, ADR-0025 (MCP boundary), ADR-0019 (structured
  dialogs rejection), ADRs 0031–0033 (TUI surface).
- Shipped evidence: `origin/291` (PR #308, inbox v0.8.0) —
  `plugins/inbox/src/cli.rs` `cmd_capability` and
  `docs/contracts/inbox-v1.md` "Capability document".

External standard status was re-verified on **2026-11-28** (§2, §3).

## 1. ACP decision — per-path table with overturn triggers

The #290 hypothesis (REJECT for the plugin/hoxay surface; conditional
upstream/hoxay-core candidate only) is **confirmed and strengthened**.
New evidence since #290 makes the ecosystem *more* mature, which
removes the "standard too young" objection but does not change the
ownership conclusion: ACP's value accrues to whoever owns the agent
execution session, and that owner is herdr core, not a plugin.

### 1.1 Current standard / ecosystem status (checked 2026-11-28)

- ACP protocol stable version is **`1`**; the repo publishes
  `schema/v1` and `schema/v2` JSON Schema artifacts plus Rust and
  TypeScript crates (`agent-client-protocol-schema`,
  `@agentclientprotocol/sdk` — SDK docs show v2.12.0 with an
  `experimental/v2` Preview entrypoint). Sources:
  <https://github.com/agentclientprotocol/agent-client-protocol>
  (README, "Versioning"), <https://agentclientprotocol.github.io/typescript-sdk/>.
- Adoption is broad on both sides: editors/clients include Zed
  (originator), JetBrains (Junie), Neovim (CodeCompanion, avante),
  Emacs (agent-shell), plus in-progress Eclipse/Toad; the curated
  registry lists ~40+ agents including Claude Code (via adapter),
  Codex CLI (`codex-acp`), Gemini CLI, Cline, GitHub Copilot (public
  preview), Goose, OpenCode, Kimi CLI, Qwen Code. Sources:
  <https://zed.dev/blog/acp-progress-report> (2025-10-02),
  <https://agentclientprotocol.com/get-started/registry>,
  <https://github.com/agentclientprotocol/agent-client-protocol>.
- Consequence: "ACP is immature" can no longer be cited as a reason;
  the rejection now rests *purely* on ownership/no-uncovered-value,
  which is the stronger and more durable ground.

### 1.2 Per-path decision table

| Path | Decision | Evidence / rationale | Overturn trigger (what NEW evidence flips it) | Owner if overturned |
| --- | --- | --- | --- | --- |
| P1. Plugin-owned ACP provider runtime (agent inside a plugin) | **REJECT — permanent under current invariants** | S6 in #290: a provider runtime inside a plugin violates AGENTS.md §4 (herdr sole execution authority; no provider runtime; honest `agent_status`/`blocked`) and ADR-0019 decision 2. ACP maturity does not weaken this. | Only if herdr core itself delegates provider hosting to plugins — i.e. an upstream ADR reversing herdr's PTY-ownership model. Not realistically obtainable; treat as closed. | n/a (would be an upstream herdr decision first) |
| P2. Plugin exposing an ACP *agent* surface to editors | **REJECT** | S5 in #290: ACP clients are editors driving *agents*; plugins are tool/contract surfaces. MCP (`rebuildup.mcp`) already covers "outside agent consumes plugin contracts" with a stable contract, access levels, and stale guards. A second protocol duplicates S4 with no new user value. | A real user scenario where an ACP-speaking editor must drive a *plugin* (not a provider agent) AND MCP demonstrably cannot express it (e.g. editor-native `session/request_permission` UX for plugin mutations). Cheaply checkable: survey herdr-t3/IDE integrations; none known today. | This repo, via ADR (maintainer) |
| P3. hoxay TUI/GUI or hoxay-core driving provider agents via ACP | **REJECT for hoxay-plugins; candidate for hoxay core / herdr upstream** | S1–S3 in #290: every valuable ACP scenario needs an ACP *session owner*. herdr already owns provider execution as PTYs and exposes `agent.*`/`events.*`; ACP would create a parallel execution authority that cannot honestly report lifecycle/`blocked`. Structured dialogs (S3) are blocked upstream (herdrdev/herdr#6196) regardless of protocol. ACP is now mature enough that "wait for the standard" is no longer the blocker — the blocker is purely *who owns the session*. | (a) herdr upstream accepts an ACP session channel as a first-class agent transport (new herdrdev/herdr Issue/ADR); (b) a provider class emerges that herdr cannot host as a PTY but users require; (c) herdr's dialog/event API lands in a form ACP maps onto cleanly. (a) is the realistic flip and is an **upstream product decision**, not obtainable from this repo. | Maintainer via ADR here for the plugin surface; herdrdev/herdr upstream Issue for core ACP support; hoxay-core responsibilities go to the sibling repo's Issues (per #297) |
| P4. ACP client adapter bridging *external* ACP agents into herdr's agent model | **DEFER — recorded boundary only, no implementation** | This is the "minimal boundary" already recorded in #290 §3: an ACP *client* adapter owned by hoxay core / herdr core that keeps the socket API as state authority, spawns ACP provider processes only for agents herdr does not host, and is compiled only when configured. It is theoretical until P3's trigger (a) fires; building it now would add a process/IPC/init path with zero current user value, violating the zero-load-when-unused spirit even if technically gateable. | Same as P3 trigger (a), plus the #290 verification plan (§3 there) executed: ≥2 real ACP providers end-to-end in an isolated named session, perf delta vs PTY path on the #287 baseline, pinned schema revision + unsupported-feature matrix, zero-load proof. | hoxay core / herdr core — **not a plugin, not this repo's implementation scope** |

### 1.3 Decision owner and next action

- The adopt/reject decision for the plugin/hoxay surface belongs to
  the **maintainer via ADR** (per #290). This document supplies the
  evidence; it does not decide.
- No ACP implementation ticket is proposed for this repo. The only
  ACP-shaped follow-up is an **optional upstream candidate Issue
  against herdrdev/herdr** ("evaluate ACP as an optional agent
  transport owned by herdr core") — same pattern as #6196/#6197/
  #6199. Filing it is a coordinator/maintainer call; hoxay-core
  responsibilities route to the sibling repo per #297.
- Cheaply obtainable evidence already gathered here: standard
  maturity, SDK availability, and ecosystem adoption (§1.1). The
  remaining evidence (real-provider interop, perf, ownership
  sanction) is *not* cheaply obtainable and correctly stays gated
  behind the overturn triggers above.

## 2. MCP convergence — gap list and test plan

Boundary is settled (ADR-0025 + #290 §4): `rebuildup.mcp` stays a
per-client stdio JSON-RPC server, contract `herdr-plugins.mcp/v1`
stable, zero load when unused. Wave 3 owns **conformance, not
redesign**. The gaps below are against the *current* MCP standard,
which has moved since the server was built.

### 2.1 Standard status (checked 2026-11-28)

- The server negotiates `2024-10-07` → `2025-06-18`
  (`plugins/mcp/src/server.ts` `SUPPORTED_PROTOCOL_VERSIONS`,
  newest-first, fallback to newest supported).
- MCP spec revisions since: **`2025-11-25`** (stable: tool-name
  guidance, `WWW-Authenticate` scope consent, tasks experimental,
  URL-based client registration) and **`2026-07-28` (FINAL as of
  2026-07-28)** — a major revision: stateless protocol core,
  `initialize`/`notifications/initialized` handshake **removed**,
  protocol version + client capabilities carried per-request in
  `_meta`, new mandatory `server/discover` RPC, `ping` removed,
  `subscriptions/listen` replaces `resources/subscribe`,
  deterministic `tools/list` ordering (SHOULD). A `2026-12-15`
  revision is planned but not ready.
  Sources: <https://modelcontextprotocol.io/specification/2026-07-28>,
  <https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/changelog.mdx>,
  <https://plan.modelcontextprotocol.io/releases>.
- Official conformance tooling now exists:
  `modelcontextprotocol/conformance` (scenario + wire-schema checks,
  version-aware, e.g. `tools-name-format` gated on ≥2025-11-25,
  deterministic-order check on ≥2026-07-28) and the TypeScript SDK's
  e2e requirement suite. Sources:
  <https://github.com/modelcontextprotocol/conformance>,
  <https://github.com/modelcontextprotocol/typescript-sdk>.

### 2.2 Conformance gap list

| # | Gap | Evidence | Proposed resolution |
| --- | --- | --- | --- |
| G1 | Protocol negotiation tops out at `2025-06-18`; `2025-11-25` and `2026-07-28` unsupported | `server.ts:24-30` | Decide per-revision support policy (see G5); at minimum document the supported matrix in `mcp-v1.md`. Implementing `2026-07-28` means removing/replacing `initialize` with `_meta`-carried version + `server/discover`, dropping `ping`, adding `subscriptions/listen` or documenting non-support — a real implementation ticket, sequenced vs #296 (§4) |
| G2 | No external conformance evidence — tests are in-repo contract tests only (`tests/mcp-*.test.ts`) | test suite is self-referential | Run the official `modelcontextprotocol/conformance` harness (or an equivalent in-repo harness modeled on it) against `serve` for every negotiated revision; wire results into `validate:fast`/CI as a non-blocking-then-blocking gate |
| G3 | `tools/list` hygiene vs 2025-11-25+ rules not verified: tool-name format (`^[A-Za-z0-9_.-]{1,128}$`), deterministic ordering (2026-07-28 SHOULD), `title`/`icons`/`_meta`/`outputSchema` fields unused | `tools.ts` definitions advertise `name`/`description`/`inputSchema`/`annotations` only | Add a schema-validation test asserting every tool's `inputSchema` is valid JSON Schema 2020-12 and names match the spec rule; decide whether to emit `title`/`outputSchema`/`destructiveHint`/`idempotentHint` annotations (additive contract extension) |
| G4 | "公開 manifest / schema / examples" deliverable absent: no machine-readable tool catalog, no published client-config examples beyond README snippet | README has one `mcpServers` JSON snippet only | Publish a generated/static tool catalog (JSON) + per-client config examples (Claude Code, Codex, generic) under `docs/contracts/` or `plugins/mcp/examples/`; keep it generated from the tool table so it cannot drift |
| G5 | SDK question unresolved: hand-rolled ~240-line JSON-RPC loop vs official `@modelcontextprotocol/sdk` | zero-dependency posture (program #286: zero added deps; ADR-0003 runtime) | **Decision record, not code**: keep the hand-rolled core for the zero-dep invariant; the official conformance suite is the interop proof. Revisit only if 2026-07-28 features (tasks, elicitation, `subscriptions/listen`) become required — then weigh SDK vs hand-rolled per-feature |
| G6 | Error-model mapping vs spec expectations partially verified | contract documents `isError` + structured codes; spec SEP-1303 wants input-validation failures as tool-execution errors (already aligned) | Conformance suite will exercise this; add explicit cases for `-32602` vs `isError` boundary in the harness |

### 2.3 Proposed conformance test plan

1. **Harness**: spawn `bun run src/cli.ts serve --access-level
   read-only --socket <fake>` (existing fake-socket pattern from
   `tests/mcp-*.test.ts`) and drive it with an MCP client. Phase A:
   in-repo scenario tests mirroring the official conformance
   scenarios (initialize/ping/tools-list/tools-call/resources,
   wire-schema validation of every message). Phase B: run the
   official `modelcontextprotocol/conformance` runner against the
   server in CI once the harness stabilizes.
2. **Version matrix**: one test leg per negotiated revision
   (`2024-10-07`…`2025-06-18` today; +`2025-11-25`, +`2026-07-28` if
   G1 lands). Unsupported-version request must negotiate down
   honestly — already implemented, needs a pinned test.
3. **Access-level matrix**: conformance legs at `read-only` and
   `supervised` asserting `capability_denied` below floor and no
   state change (extends existing `mcp-mutations.test.ts`).
4. **Regression anchor**: the harness must be **black-box over the
   `serve` argv/stdio contract**, so it survives the TS→Rust rewrite
   in #296 unchanged (see §4).

## 3. #263 `command_metadata` — post-Wave-1 status and remainder

### 3.1 What shipped (Wave 1, PR #308 / `origin/291`)

Inbox v0.8.0 `capability` now emits `command_metadata` exactly per
the #290 §5 minimal subset:

- `kind`: `read` for `list`/`links`/`transitions`/`capability`/
  `help`; `mutation` for the 13 `mutation_commands` entries.
- `idempotent: true` on all mutations except daemon-class
  `watch`/`watch-pr` (field omitted — excluded from retry semantics,
  matching the ledger).
- `coalesce: true` on the four pure reads; `freshness:
  event-driven` on `list`/`links`, `snapshot-consistent` on
  `transitions`; `invalidation: "pull"` on the three state reads.
- `commands`/`mutation_commands` unchanged (backward compatible);
  `inbox-v1.md` documents the section as additive with
  absent⇒conservative-defaults consumer rule.

This matches the ledger's minimal table field-for-field. **The
minimal subset is correctly implemented; #263 is not closed** (it is
the contract-SoT ticket and stays open per #290's instruction).

### 3.2 What remains for full #263 closure

| # | Remaining work | Where it belongs |
| --- | --- | --- |
| R1 | Promote `command_metadata` from inbox-v1's local section into the **shared capability-advertisement policy** in `docs/contracts/README.md` (field semantics, optional-consumption rule, daemon-class `idempotent` omission, `"<cmd> <sub>"` key convention for subcommand-scoped mutations) so every plugin uses one shape | Contract-SoT work — #263's own delivery or a small dedicated ticket; do **not** fork a second metadata design |
| R2 | Per-plugin adoption: every CLI plugin's `capability` adds `command_metadata` for its commands. Highest value first: expensive/index reads (`conversations`, `usage`, `git` reads — `coalesce`/`freshness` matter most), then the rest. Subcommand-scoped mutations (`"repos add"` form per #224) need the R1 key convention settled first | Each plugin's own ticket — Wave-2 native ports (#292/#293/#294…, PRs #309–#311) are the natural carrier since they already freeze the contract; this ticket only coordinates |
| R3 | `mcp` plugin: no `command_metadata` (no per-feature CLI; gating is per-tool `access`/`read_only` per mcp-v1) — record the equivalence note in the contract-SoT update so consumers don't expect the field there | R1 doc work |
| R4 | Consumer side (herdr-t3 opportunistic read, TTL-cache adoption, retry policy) | **Sibling repo** — herdr-t3 Issues per #263's consumer section; not this repo |
| R5 | Verification: contract test per adopting plugin asserting metadata covers every `commands`/`mutation_commands` entry and kind agrees with `mutation_commands` | Each adoption ticket |

**Coordination note:** Wave-1 plugins other than inbox (usage #294,
git #292, conversations #293 — PRs #309–#311 open) have *not* shipped
`command_metadata` on their branches as of this writing (verified:
`git grep command_metadata` across `origin/291` and
`origin/release-0-13-0` hits only inbox). Whether each Wave-2 ticket
adds it is a per-ticket scope decision for the coordinator; this
document recommends adopting it during each plugin's contract-freeze
step rather than a big-bang migration, matching #290 §5.

## 4. Sequencing vs Wave 2 (#296)

#296 owns the Rust/native migration of the remaining plugins,
including `rebuildup.mcp` (Bun removal). Wave 3 must not implement
MCP changes that #296 will rewrite, and #296 must not ship a server
that Wave 3 immediately has to re-conform. Recommended order:

1. **Now (this ticket):** decision records only — this document.
   No `plugins/*/src` changes.
2. **Contract/spec layer (can land before #296's mcp port):**
   - `command_metadata` contract-SoT update (R1) — implementation-
     agnostic, benefits every plugin regardless of runtime.
   - Public tool catalog + client examples (G4) — generated from the
     contract, runtime-agnostic.
   - Conformance *harness specification* — define scenarios/assertions
     in `docs/specs/mcp.md` or a new spec section; the harness itself
     is black-box over `serve` argv so it ports to Rust for free.
3. **Protocol revision support (G1) — coordinate, default = after
   #296's mcp port:** implementing `2025-11-25`/`2026-07-28` inside the
   TS implementation that #296 will delete is wasted work. Preferred:
   #296 ports current behavior 1:1, Wave-3 child ticket then adds new
   revisions once, in Rust. If #296's mcp ticket stalls, the revision
   work may land in TS first *only if* the coordinator accepts the
   double-implementation cost — flag as an open question (§6).
4. **Conformance harness execution (G2/G3/G6):** can start against
   the current TS `serve` immediately (black-box), giving #296 a
   regression gate for the port — this is the cheapest way to satisfy
   "非劣化" for mcp and should be scheduled *before* the port merges.

## 5. Proposed child issues (coordinator files; drafts only)

| # | Title (draft) | Scope | Acceptance (draft) |
| --- | --- | --- | --- |
| C1 | `contract: command_metadata を capability 広告ポリシーへ昇格し Wave-2 各 plugin へ展開` | R1 + R2/R3/R5 coordination: update `docs/contracts/README.md` policy section with the #263 field semantics + `"<cmd> <sub>"` key rule + mcp equivalence note; add per-plugin adoption checklist; each Wave-2 plugin ticket adds its own `command_metadata` + coverage test | README policy documents all five fields + consumption rule; every CLI plugin's `capability` emits metadata covering `commands`/`mutation_commands`; `mcp-v1.md` records the per-tool-gating equivalence; per-plugin contract tests assert coverage/kind agreement; #263 can then be evaluated for closure by its owner |
| C2 | `mcp: 公式 conformance ハーネスによる stdio server 適合試験を CI 化` | G2 + G4 + G6: in-repo black-box harness over `serve` argv (Phase A scenarios + wire-schema validation), then official `modelcontextprotocol/conformance` runner; publish generated tool catalog JSON + per-client config examples | Every negotiated revision leg passes initialize/tools-list/tools-call/resources + access-level denial cases; tool names/schemas validated against spec rules; catalog/examples committed and drift-checked; harness runs in `validate:fast` or CI and survives the #296 Rust port unchanged |
| C3 | `mcp: MCP 2025-11-25 / 2026-07-28 revision 対応の採否と実装` | G1: decide support matrix per revision (document in `mcp-v1.md`); implement chosen revisions — for 2026-07-28: `_meta` version/capabilities, `server/discover`, `ping` removal handling, `subscriptions/listen` or documented non-support, deterministic `tools/list` | `mcp-v1.md` records the supported-version matrix; each supported revision passes C2's harness; unsupported-version negotiation remains honest; zero-load-when-unused unchanged; lands after #296's mcp port unless coordinator rules otherwise (§6) |
| C4 | `research: herdrdev/herdr への ACP session-ownership upstream Issue 起票判断` | Optional: file the P3/P4 overturn-path candidate against herdrdev/herdr (evaluate ACP as optional agent transport owned by core); record outcome back into `docs/research/` | Issue filed or explicitly declined with rationale recorded; no code in this repo |

Deliberately **not** proposed: any ACP implementation ticket, any
SDK adoption ticket (G5 is a recorded decision to keep zero-dep),
any `plugins/*/src` change under this ticket.

## 6. Open questions for the maintainer/coordinator

1. **MCP revision timing vs #296:** should `2025-11-25`/`2026-07-28`
   support land in the current TS implementation before the Rust
   port (accepting double work), or strictly after (recommended,
   §4.3)? This is a sequencing/product call, not a technical one.
2. **`command_metadata` ownership:** is promoting the field into the
   shared contracts README policy #263's own remaining scope, or a
   new ticket (C1)? #263 stays open either way; only the carrier
   ticket differs.
3. **ACP upstream Issue:** file C4 now as a tracking candidate, or
   wait for a concrete user scenario demanding ACP-driven agents
   herdr cannot host? Recommendation: wait — the overturn triggers in
   §1.2 are all upstream-gated, and a bare "support ACP?" Issue
   without a scenario repeats the #290 finding with no new evidence.
4. **SDK/examples depth:** is the generated tool catalog + config
   examples (C2) sufficient for "公開仕様" this wave, or does the
   maintainer want a published JSON Schema document per tool
   (heavier, only if external consumers ask)?

## 7. What later work can rely on

- ACP: rejection confirmed with ecosystem-maturity evidence; overturn
  path is trigger-defined (§1.2); nothing in this repo implements or
  schedules ACP.
- MCP: boundary unchanged; conformance is the work — harness first
  (C2, implementation-agnostic), revision support second (C3,
  sequenced vs #296), catalog/examples as part of C2.
- #263: minimal subset shipped correctly in inbox v0.8.0; remainder
  is contract-SoT promotion + per-plugin adoption (C1) + sibling-repo
  consumer work — no duplicate tickets created here.
