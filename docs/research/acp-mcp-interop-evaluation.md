# Research: ACP adoption hypothesis and MCP preservation assessment (Issue #290, Program #286)

Status: decision-input research for the Native Performance program
(#286). This document is **non-blocking**: it produces the ACP
adopt/reject hypothesis, the MCP preservation assessment, and the
per-scenario ownership/load evaluation that Wave 1 (#291) and Wave 3
(#297) consume. It implements nothing and closes nothing.

Pins and authorities:

- Program plan: GitHub Issue #286 (Revision 2 + Addendum + third-party
  re-review Go, maintainer-approved 2026-11-27).
- Prior boundary decisions: ADR-0019 (#102, structured dialogs),
  ADR-0025 (#135, MCP outside-agent boundary), ADRs 0031–0033 (#228/#229,
  TUI surface).
- Upstream inventories: `docs/research/mcp-upstream-inventory.md`
  (t3code `9cc4018c`, herdr 0.9.5 / API protocol 22),
  `docs/research/secrets-remote-devices-inventory.md`.
- Filed upstream gaps already on record: herdrdev/herdr#6191
  (`agent_session_path`, closed NOT_PLANNED), #6196 (structured
  dialogs), #6197 (message admission / queue-steer), #6199 (remote
  MCP HTTP/OAuth), #6200-class webhook gap per ADR-0026.
- Standards: Agent Client Protocol (ACP,
  <https://agentclientprotocol.com>) and Model Context Protocol (MCP,
  <https://modelcontextprotocol.io>). Exact spec revisions are pinned
  at adoption time; this document fixes *whether* and *where*, not the
  byte-level schema.

## 1. Design constraint applied to every scenario

Program #286 invariant: **zero added dependencies, spawned processes,
IPC, or initialization when the capability is unused**, with the
dependency isolated behind the optional boundary. A plugin must never
own a provider runtime. Any scenario that needs one is recorded as a
candidate separate hoxay-core / herdr-upstream issue, not implemented
here.

## 2. Per-scenario evaluation

Scenarios are written from the real user/operator point of view, not
from protocol features.

| # | Scenario | Value | Ownership | Standard | Added load when unused / when used |
| --- | --- | --- | --- | --- | --- |
| S1 | An editor or external agent wants to drive a herdr-managed provider agent through a structured protocol (ACP) instead of PTY/CLI text | Medium–high *if* herdr core ever speaks ACP; today herdr owns the agent as a PTY and already exposes `agent.*`/`events.*` over its socket API. ACP would only add value for providers herdr cannot host — none exist in scope | **herdr core / hoxay core**, never a plugin (plugins must not own provider runtimes; ADR-0019 already rejected plugin-side ACP) | ACP spec (pin at adoption) | Unused: zero — no dep, no spawn, no IPC. Used: one ACP child process per agent session *in addition to* the herdr PTY → double execution authority; unacceptable until herdr core owns the ACP session |
| S2 | A client wants structured agent events (turn updates, tool calls, permission requests) rather than PTY scraping | Already covered: herdr `events.subscribe`/`events.wait` + `session.snapshot` expose lifecycle/agent-status events; `rebuildup.mcp` exposes them to outside agents; conversations normalizes transcript history | herdr socket API (authority) + `rebuildup.mcp` (adapter) | herdr socket API; MCP for external clients | Unused: zero. Used: existing socket subscription only; no new process |
| S3 | hoxay TUI/GUI needs structured dialogs (approval/question/attachments/goals) — the #102/#8 gap | Real user value confirmed, but the blocker is *upstream*: herdr has no dialog API (`agent.prompt` rejects `blocked`; no structured dialog events). ACP does not fix this because herdr's providers run as TUI PTYs, not ACP sessions | herdr upstream (herdrdev/herdr#6196 filed); fallback today is PTY interactive fallback per ADR-0019 | n/a until upstream lands | Unused: zero. Used (hypothetical ACP path): provider runtime inside plugin → **rejected**, would violate the no-provider-runtime invariant |
| S4 | An outside agent (terminal Claude Code/Codex, IDE agent, bot) operates herdr sessions and plugin contracts | High — already delivered: `rebuildup.mcp` v0.7.0, contract `herdr-plugins.mcp/v1` **stable**, access levels, stale-generation guards, event subscriptions with honest gap/resync | `rebuildup.mcp` plugin (protocol adapter only; execution stays with herdr) | MCP 2024-10-07–2025-06-18 (version negotiated per client, already implemented) | Unused: **zero** — the server exists only as a per-client stdio process spawned from the user's MCP client config; nothing runs at herdr/plugin startup (no `[[startup]]`/`[[events]]`, ADR-0002) |
| S5 | A plugin exposes its contract to ACP-speaking clients | Low: ACP clients are editors driving *agents*, not general tool surfaces; MCP already covers the "outside agent consumes plugin contracts" case with a stable contract and access model | would be `rebuildup.mcp`'s territory if ever needed; no plugin owns this | ACP (as client-side of the editor) | Unused: zero. Duplicates S4's value with a second protocol → not justified |
| S6 | A plugin embeds an ACP provider runtime to get structured control of a provider | Rejected outright | would be a provider runtime — forbidden by AGENTS.md invariants and ADR-0019 decision 2 | ACP | Forbidden: permanent added process + bypasses herdr PTY authority, `agent_status` honesty, and lifecycle ownership |

## 3. ACP adopt/reject hypothesis

**Hypothesis: REJECT ACP for hoxay-plugins and for the current hoxay
surface; record a conditional upstream/hoxay-core candidate only.**

Rationale:

1. **Ownership conflict.** Every valuable ACP scenario (S1–S3)
   requires an ACP *session owner*. herdr already owns provider
   execution as PTYs; a second ACP channel would create a parallel
   execution authority that cannot honestly report `agent_status`,
   `blocked`, or lifecycle state (ADR-0019 decision 2, unchanged).
2. **No uncovered user value.** Structured events (S2) and outside-agent
   operation (S4) are already served by the herdr socket API and the
   shipped `rebuildup.mcp`. Structured dialogs (S3) are blocked
   upstream regardless of protocol — ACP cannot substitute for a herdr
   dialog API.
3. **Cost asymmetry.** Adoption means per-agent ACP child processes,
   a new protocol dependency, and conformance surface — all for
   scenarios herdr core would have to own anyway. The zero-load
   constraint is satisfiable but the *used*-path cost is pure
   duplication today.

**Minimal boundary if the hypothesis is ever overturned** (recorded so
a future ticket does not re-derive it): an ACP *client* adapter owned by
**hoxay core / herdr core** — never a plugin — that (a) keeps herdr's
socket API as the state authority, (b) spawns an ACP provider process
only for agents herdr does not already host, (c) reports ACP session
state *through* herdr's agent model rather than beside it, and (d) is
compiled/loaded only when configured (zero load when unused). Until
herdr core sanctions such a channel, this boundary is theoretical.

**Decision owner:** the adopt/reject decision belongs to the
**maintainer via ADR** in this repo for the plugin/hoxay surface, with
any herdr-core ACP support tracked as a **separate upstream Issue
candidate against herdrdev/herdr** (same pattern as #6196/#6197/#6199).
This research doc is the evidence input, not the decision.

**Verification plan (required before any future adoption, per #286):**

1. Real-provider interop: at least two ACP providers (e.g.
   `claude-code-acp`, `codex-acp`, or the then-current official ACP
   agents) driven end-to-end through the candidate boundary in an
   isolated named session.
2. Performance/load measurement on the program's fixed baseline
   (#287): process count, IPC bytes, init latency, RSS delta vs the
   equivalent PTY path; ACP-disabled vs enabled-unconnected vs
   connected measured separately.
3. Standard conformance: pinned ACP schema revision, protocol-version
   negotiation record, and a documented unsupported-feature matrix
   (mirroring `controls-v1`'s `capability.dialogs` honesty pattern).
4. Zero-load proof: with the feature unconfigured, `bun`/binary starts
   no ACP process and links/loads no ACP code path.

## 4. MCP assessment (confirm, do not change)

Existing functionality is already aligned with the program's MCP
requirements; **#290 changes nothing in `rebuildup.mcp`.**

- **Preservation:** contract `herdr-plugins.mcp/v1` is **stable**
  (read surface T2/#138, mutations T3/#139, events T4/#140); tools,
  access levels, stale-generation guards, and gap/resync semantics are
  contract-tested (`tests/mcp-*.test.ts`) and live-verified
  (`docs/research/mcp-v1-live-verification.md`).
- **stdio-first:** already the only v1 transport — per-client stdio
  JSON-RPC spawned from user MCP client config; no resident process,
  no daemon, no inbound HTTP/OAuth (remote is the filed upstream gap
  herdrdev/herdr#6199, out of scope by ADR-0025).
- **Permission semantics:** `--access-level read-only|supervised|full`
  enforced server-side; mutations below `supervised` fail with
  structured `capability_denied`; provider permission mode capped via
  controls launch args (ADR-0018); `expect_*` stale guards forwarded.
- **Registration / disable / version:** registration is user-owned MCP
  client config (the plugin ships no config writes); disable = remove
  the client entry or uninstall the plugin — no herdr state involved;
  versioning follows the contracts README policy (additive extends v1,
  breaking → `/v2`).
- **Load:** zero when unused — nothing starts at plugin install or
  herdr startup; the server process lifetime equals the MCP client's.
- **Wave 3 (#297) owns:** MCP optimization, public spec/conformance
  tests, and any needed SDK — consuming this assessment, not
  re-deciding the boundary.

## 5. What later waves can rely on

- Wave 1 (#291 Inbox): no ACP/MCP dependency; the only contract input
  is the #263 `command_metadata` minimal subset (see
  `docs/research/disposition-ledger.md` §5).
- Wave 3 (#297): MCP boundary is settled (ADR-0025 + this §4); ACP is
  a recorded rejection with a defined overturn path (§3); remaining
  work is optimization and conformance, not adoption research.
