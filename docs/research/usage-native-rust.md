# Research: native Rust port of rebuildup.usage (Issue #294)

Implementation + verification record for porting `plugins/usage`
from TypeScript (Bun) to a committed native binary (Rust), per ADR-0035
(native plugin distribution) and ADR-0036 (native TUI runtime). Program
#286 / native-performance-baseline (#287) motivated the port; baseline
numbers live in `docs/research/native-performance-baseline.md`. This
follows the `rebuildup.conversations` port (#293,
`docs/research/conversations-native-rust.md`).

Verification host: macOS arm64 (Apple M5), herdr `0.9.5`
(`/Users/rebui/.local/share/mise/installs/herdr/0.9.5/herdr`). Linux and
Windows are **unverified** — `platforms = ["macos"]` in the manifest is
the honest scope until binaries for other targets exist and are verified.

## 1. What shipped

| piece | location | notes |
| --- | --- | --- |
| plugin binary | `plugins/usage/bin/usage` (~4.3 MB, committed, macOS arm64) | manifest argv is `./bin/usage`; no Bun/toolchain needed at install or run time |
| domain + CLI | `plugins/usage/src/{model,aggregate,cost,divergence,pricing,quota,hub,state,source,cli}.rs` | contract `herdr-plugins.usage/v1` stable; version `0.6.0` |
| TUI | `plugins/usage/src/tui.rs` | contract `herdr-plugins.tui/v1`; in-process `cli::run_domain` calls on worker threads (no self-respawn), same pattern as `plugins/inbox`/`conversations` |
| shared crates | `crates/herdr-client`, `crates/tui` (`safety`) | reused unchanged |
| TS implementation | deleted (`plugins/usage/src/*.ts`, 5 retired test files) | recoverable at `6614811` (= pre-removal `origin/293` head) |

Manifest (`herdr-plugin.toml`) entrypoints: `[[panes]] main` →
`./bin/usage tui main`, `[[actions]] open` → `./bin/usage tui --open`.
The plugin's conversations delegation (`source.rs`) resolves
`rebuildup.conversations` and spawns `bin/conversations` first with a
`bun src/cli.ts` fallback — the ADR-0035 pattern. Because usage's own
`src/cli.ts` is gone, `plugins/textgen/src/pluginroot.ts` was updated to
the same bin-first resolution so its quota-guard probe of
`rebuildup.usage` keeps working (`isPluginRoot`/`runPluginCli` now prefer
`bin/<pluginDirName>`, mirroring `plugins/mcp/src/plugincli.ts`).

## 2. Contract parity evidence

- `tests/usage-native.test.ts` — 19 spawn-based contract tests against
  the committed binary: capability, usage aggregation (claude+codex
  fixtures, selectors/windows, empty+missing honesty), quota contract
  (cache hit/refresh, request_failed→stale, credential_expired,
  no_credential, apikey→unsupported), cost (unpriced/pricing-down/
  modelPrices override), divergence, cliproxy hub quota source, and the
  non-TTY `tui` fallback. Helper: `tests/helpers/usage-bin.ts` (locates
  `plugins/usage/bin/usage`, `target/debug|release` fallback, scrubs
  `HERDR_USAGE_*`).
- `cargo test -p usage` — 32 unit tests covering aggregation math,
  window/filter semantics, quota/hub cache TTL+GC, pricing merge,
  divergence verdicts, paging/cursor handling, and the
  `run_with_timeout` reader-thread path.
- `scripts/diff-usage.ts` — fixture-driven TS↔Rust output-parity
  harness. `git archive`s `plugins/usage` + `plugins/conversations` at
  the pinned pre-removal commit `6614811`, runs both implementations
  against identical fixtures and fresh state dirs, and deep-compares
  normalized stdout JSON, exit codes, and cache-file bytes. Result:
  **33/33 checks identical** (usage-errors ×4, capability, usage
  aggregation ×5, quota ×5, cost ×2, divergence ×3, hub-quota ×4,
  tui-fallback ×2, plus per-scenario cache parity). Re-run:
  `bun scripts/diff-usage.ts`.

## 3. Bugs found by the port (all fixed)

1. **`run_with_timeout` pipe-buffer deadlock** (`source.rs`) — the first
   port used `wait_with_output()`, which only drains stdout/stderr
   after the child exits. A `conversations usage` page emitting more
   than the ~64 KiB pipe buffer blocked on write while the parent
   blocked in `wait`, so the 40 s timeout fired and the whole export
   reported `conversations_error: timed out or failed to spawn` (the
   full 10k-event fixture never completed). Rewritten to spawn reader
   threads draining both pipes while `try_wait` polls the deadline —
   the same pattern `plugins/inbox` uses. After the fix the full export
   completes in ~1.1 s (1 651 events, 4 pages).
2. **TUI `poll_load` consumed its own result** (`tui.rs`) — the
   non-blocking poll probed readiness with `rx.try_recv()` (which
   *removes* the value on `Ok`) and then called `try_recv()` again,
   which always returned `Empty` → every successful report load was
   misreported as `error: load failed`. Rewritten as a single
   `try_recv` match (`Ok` → finish, `Empty` → keep waiting,
   `Disconnected` → honest failure). Caught by the np294 PTY drive,
   not by unit tests — the interactive load path had no coverage.
3. Test-side fixes during bring-up: two Rust divergence tests compared
   `json!(30)` vs `json!(30.0)` (serde `Number` equality is
   representation-sensitive) and one window-span assertion used the
   wrong millisecond arithmetic; both corrected, no product change.

## 4. Isolated-session verification (np294)

Dedicated XDG/herdr session `np294`
(`XDG_CONFIG_HOME=/var/tmp/np294-herdr/config`, etc.; the user's live
sessions were never touched — every herdr CLI call pinned
`HERDR_SOCKET_PATH` to the np294 socket):

- `herdr plugin link plugins/usage` — manifest parsed; `plugin list`
  shows the `open` action (`./bin/usage tui --open`), the `main` pane
  entrypoint (`./bin/usage tui main`), `platforms=["macos"]`,
  `version 0.6.0`.
- `capability`, `usage --offline`, `quota`, `cost --offline`,
  `divergence --offline` against the real socket: honest envelopes —
  `usage` reports `events: 0` with the empty index, `quota` probes the
  real credential stores read-only (claude → `credential_expired`,
  codex → `ok` plan `Plus` with live window percentages, opencode →
  unsupported), `cost` labels the LiteLLM estimate and prints the
  not-a-bill disclaimer, `divergence` reports `no_data` /
  `no_measured_usage` verdicts side by side.
- `herdr plugin action invoke open --plugin rebuildup.usage` spawned
  `./bin/usage tui --open`, exit 0; a real `Usage` pane (`w1:p2`)
  opened in the np294 session and `herdr pane read` shows the
  reports picker (`usage / quota / cost / divergence`) rendered.
- `herdr plugin disable`/`enable rebuildup.usage` toggled cleanly.
- **PTY visual evidence** (`spikes/usage-tui/drive.py`, pyte screen
  snapshots in `spikes/usage-tui/evidence/`): the np294 conversations
  plugin state dir was seeded with a read-only copy of the np287 perf
  fixture (25 sessions / 10k events / 1 651 usage events),
  `HERDR_SOCKET_PATH` pinned to the np294 socket. The `main` scenario
  captured: reports picker, populated usage report (totals, cache
  tokens, coverage, provider/model breakdowns), scroll, `w` window
  cycling (24h → empty-honest), `p` provider filter, `b` breakdown
  dimension picker → rows → JSON detail → back, quota report (real
  provider statuses), quota detail JSON, cost report ($525.09
  estimate, labeled), divergence report; clean `q` quit (exit 0).
  Human visual review of these snapshots remains a separate
  acceptance gate — the PR stays Draft until then.
- **Not exercised live:** hub (cliproxy) quota sources and
  `modelPrices` config overrides — covered by the contract suite's
  fake-socket/mock fixtures instead.
- `herdr plugin install
  rebuildup/hoxay-plugins/plugins/usage --ref 294 --yes` — managed
  GitHub install verification is recorded in the PR body after push
  (same flow as #293 §4).

## 5. Performance re-measurement (#287 baseline comparison)

Same host, same canonical fixture
(`/var/tmp/np287-herdr/state/herdr/plugins/rebuildup.conversations` —
25 sessions / 10,000 normalized events / ~5.5 MB `events/*.jsonl`; the
small variant is the 3-session/600-event fixture under
`/var/tmp/np287-small`), same harness (`scripts/perf/measure.ts`,
`scripts/perf/tui-probe.py`), warm page cache, shared machine. All runs
used a scrubbed `env -i` environment (the measuring shell itself sits
inside a herdr pane, so ambient `HERDR_*` was fully cleared — the #287
baseline ran from a clean shell). Raw JSON:
`docs/research/native-performance-baseline/50`–`56-*.json`.

| operation | TS baseline (Bun) | native `bin/usage` | delta |
| --- | --- | --- | --- |
| `capability` p50 (n=30) | — (not measured) | 18 ms | — |
| `usage --offline --provider opencode` p50 (full index) | 235 ms | 438 ms | **~0.54× — slower, see notes** |
| ↳ max RSS (n=10) | ~35 MB | 13.4 MB | ~2.6× smaller |
| `usage --offline` p50 (small fixture) | 393 ms | 420 ms | ~0.93× — parity |
| `usage --offline` p50 (full fixture, unfiltered) | >30 000 ms timeout (defect D1) | **1 074 ms, 30/30 runs succeed** | unbounded → bounded |
| TUI `first_byte` p50, small (n=12) | 111 ms | 12 ms | ~9× |
| TUI `data_usable` p50, small | 585 ms | 442 ms | ~1.3× |
| TUI pgid peak RSS p50, small | ~71 MB | ~9.7 MB | ~7× smaller |
| TUI `first_byte` p50, full index | 73 ms | 13 ms | ~5.6× |
| TUI `data_usable` p50, full index | **never** (>110 s, stuck `loading…`) | **1 110 ms, 12/12 markers** | unbounded → bounded |
| TUI pgid peak RSS p50, full index | ~210–309 MB and climbing | ~21–27 MB | ~10× smaller |

Notes:

- **The opencode-filtered CLI path is ~1.9× slower in wall time**
  (438 ms vs 235 ms p50). The native binary delegates to
  `bin/conversations usage --offline` (~120 ms per invocation on this
  index) and the usage side itself is single-digit-ms Rust; the gap is
  process-spawn + index-scan overhead that Bun amortized differently.
  The same absolute cost is hidden inside the TS numbers only because
  the TS baseline never reached the unfiltered path (D1). Where the TS
  implementation *did* terminate, native is at parity (small fixture)
  or dramatically better (full index, TUI big). If the per-page child
  respawn becomes the bottleneck at larger indexes, a streaming export
  or an in-process index read is the follow-up — the #287 doc already
  flagged "2 child spawns + N page spawns → 0–1" as the call-count
  reduction potential.
- **The headline win is termination, not median latency**: the
  unfiltered full-index `usage` went from a >30 s hang (and a TUI that
  never became usable, ~300 MB RSS climbing) to ~1.1 s CLI / ~1.1 s
  TUI-data at ~13–27 MB RSS. That is defect D1's fix (#293) plus this
  port's pipe-deadlock fix compounding.
- TUI `data_usable` markers were measured with `--marker
  sessions_with_usage` (the `totals:` label is split across ANSI
  writes often enough to defeat naive substring matching — a probe
  artifact, not a render defect; pyte snapshots show the full report).

## 6. Known limitations / follow-ups

- **macOS arm64 binary only.** `platforms = ["macos"]`; Linux/Windows
  need verified builds before widening the manifest — ADR-0035.
- **Committed `bin/usage` must be rebuilt on every source change**
  (`cargo build --release -p usage && cp target/release/usage
  plugins/usage/bin/usage`); no CI freshness gate yet — same follow-up
  as `bin/inbox`/`bin/conversations`.
- Per-page `conversations usage` child respawns (§5) — candidate for a
  single streaming export or direct index read if indexes grow.
- `cost`/`divergence`/`quota` network paths are exercised only via
  fixtures/mocks in tests; live provider probing was verified manually
  in np294 (§4) but is not in the automated suite (external I/O).
- `--from`/`--to` date parsing is ISO-8601-strict (`parse_js_date`),
  not full `Date.parse` semantics — the contract documents ISO-8601
  input, so non-ISO forms that JS would accept return a usage error
  here. Defensible divergence; widen only if the contract changes.
- ~~Managed-install verification~~ — recorded post-push in the PR body.

## 7. Rollback

The TS implementation is intact at `6614811`. To roll a user
installation back: `herdr plugin install
rebuildup/hoxay-plugins/plugins/usage --ref <previous-release-sha>`.
Plugin state (quota/pricing caches under
`HERDR_PLUGIN_STATE_DIR/{quota-cache,pricing-cache}`) is byte-format
compatible — the diff harness compares cache files — so no state
migration is needed either direction. Sibling plugins fall back to
`bun src/cli.ts` automatically if a checkout without `bin/` is
resolved (textgen's `pluginroot.ts` now prefers `bin/` with the same
fallback).
