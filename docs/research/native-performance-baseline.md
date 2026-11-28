# Native Performance baseline — top-5 operations (#287 / Program #286)

Performance baseline and non-regression criteria for the five priority
operations named by Program #286 (Native Performance — Rust
unification): **(a) Inbox TUI startup, (b) Usage aggregation,
(c) Conversations search, (d) Git status/diff, (e) a simple local CLI
invocation** — plus lightweight measurements of adjacent surfaces
(Conversations TUI, `sessions`/`history`/`files` reads, `inbox list`).

- Code under test: `release-0-13-0` @
  `0da65f28979fb91c3a0dbbfe96c0b1f87c8dbd9d` (v0.12.0 content; the
  `287` branch carries only `scripts/perf/` + this document).
- herdr `0.9.5` (real binary
  `~/.local/share/mise/installs/herdr/0.9.5/herdr` — the mise shim fails
  under XDG overrides, same note as prior RC docs), Bun `1.4.4`,
  git `2.56.1`, python3 `3.15.2`.
- Host: macOS 27.2.1 (arm64), Apple M5, 24 GB. **Shared development
  machine** — other agent workloads were active during measurement;
  run-to-run variance is real and quantified below.
- `hyperfine` is not installed; an equivalent in-repo harness was
  written instead (`scripts/perf/measure.ts`, `scripts/perf/tui-probe.py`)
  because it also captures RSS and separates TUI first-frame vs
  data-usable latency, which hyperfine cannot do. Re-run commands are
  listed verbatim so the choice is reproducible either way.

## Isolation

All measurements ran against dedicated fixtures and an isolated herdr
session; the user's live session/agents/worktrees were never touched.

| Resource | Location |
| --- | --- |
| herdr session | `np287`, headless `herdr --session np287 server` under `XDG_CONFIG_HOME=/var/tmp/np287-herdr/config`, `XDG_STATE_HOME=/var/tmp/np287-herdr/state`, `XDG_DATA_HOME=/var/tmp/np287-herdr/data`, `HERDR_CONFIG_PATH=/var/tmp/np287-herdr/config/herdr.toml` |
| plugin state | same `XDG_STATE_HOME`; plugin dirs resolve to `/var/tmp/np287-herdr/state/herdr/plugins/<id>` exactly as in production |
| conversations index fixture | `/var/tmp/np287-herdr/state/herdr/plugins/rebuildup.conversations/` (copied from generator output) |
| git repo fixture | `/var/tmp/np287-fixtures/git-repo` (pane cwd `w2:p1` in session `np287`) |
| ambient env | every invocation ran with `env -u HERDR_SOCKET_PATH -u HERDR_PANE_ID -u HERDR_PLUGIN_CONTEXT_JSON -u HERDR_PLUGIN_STATE_DIR -u HERDR_PLUGIN_CONFIG_DIR -u HERDR_TUI_CONTEXT_JSON` plus `XDG_*` above and `HERDR_SESSION=np287` |

## Fixtures (deterministic, regenerate anytime)

`scripts/perf/gen-fixtures.ts` (seeded `mulberry32(0x287)` — byte-stable
for a given parameter set). Fixtures are intentionally **not committed**
(multi-MB); the generator is the fixture.

```bash
bun scripts/perf/gen-fixtures.ts --out /var/tmp/np287-fixtures
# defaults: --events 10000 --sessions 25 --files 5000 --modified 120 --untracked 60
bun scripts/perf/gen-fixtures.ts --out /var/tmp/np287-small --events 600 --sessions 3 --files 50 --modified 5 --untracked 3
```

| Fixture | Content |
| --- | --- |
| conversations index | 25 sessions (`claude` ~60 % / `codex` ~30 % / `opencode` ~10 % by seeded weight), **10,000 normalized events total** (~5.5 MB `events/*.jsonl`), realistic type mix (`turn`/`user_message`/`assistant_message`+usage/`tool_call`/`tool_result`/`system`), timestamps over ~30 days, `identity` blocks per event, `state.json` registry with cursors/counters |
| git repo | 5,000 tracked `.ts` files in 100 module dirs + README/package.json, one commit on `main`, then **120 modified + 60 untracked** files left in the worktree |
| herdr session `np287` | 16 panes across 2 workspaces; 10 panes agent-reported (`idle`/`working`/`blocked` — 3 `blocked` ⇒ 3 attention items for the inbox), all `w2:*` panes `cwd` = git fixture repo |
| small variant | 3 sessions / 600 events / 50-file repo — used to isolate the conversations paging defect's scale threshold |

## Harness

- `scripts/perf/measure.ts` — spawns the command as a fresh process per
  run (`--runs N`, `--warmup M` discarded, `--rss` wraps runs in
  `/usr/bin/time -l` for max-RSS of the top-level process only), reports
  min/p50/p95/p99/max/mean/stdev as JSON. Every run is already a cold
  process start; there is no persistent runtime to warm.
- `scripts/perf/tui-probe.py` — forks a real PTY (120x40), timestamps
  first output byte (≈ first frame) and first output matching an
  ANSI-stripped `--marker` regex (≈ "data usable"), samples process-group
  RSS via `ps` every ~30 ms, then sends a quit key. `--advance-marker`/
  `--advance-key`/`--advance-delay-ms` support multi-step flows (e.g.
  open a submenu).
- Percentiles use nearest-rank on sorted samples; n=30–40 for CLI ops,
  n=10–12 for TUI ops.

### Trial conditions / known limitations

- **Cold vs warm**: every measured run is a fresh OS process (cold
  start). OS page cache is warm after the discarded warmup runs; a
  truly disk-cold run would need `sudo purge` (not automated, no
  privilege). Bun transpiles TS per process — there is no persistent
  JIT cache to invalidate.
- **Shared machine**: other agent workloads ran during measurement.
  Exploratory runs earlier the same day showed p95 up to ~1.9 s for
  `git status` and a 7.3 s outlier for `search` under heavier load; the
  canonical dataset below was captured in a quieter window. Treat
  p50 as the stable signal and p95/p99 as load-sensitive bounds.
- RSS: `/usr/bin/time -l` attributes only the top-level process
  (spawned grandchildren are not counted — material for `usage`, which
  chains two child CLIs). The PTY probe's `pgid_peak_rss_kb` sums RSS
  across the whole process group (~30 ms sampling; shared pages are
  double-counted, so it is a conservative upper bound).
- External-I/O wait vs internal CPU is **not** separable per run with
  this harness; decomposition below is inferred from spawn counts,
  file sizes, and micro-benchmarks (marked *estimated*).

## Results — CLI operations (canonical dataset)

Raw JSON: `docs/research/native-performance-baseline/*.json`.
Wall ms, fresh process per run, warm page cache.

| # | Operation (command) | n | p50 | p95 | p99 | max | failures |
| --- | --- | --- | --- | --- | --- | --- | --- |
| e | `bun -e 0` (runtime floor, reference) | 40 | 24 | 30 | 37 | 37 | 0 |
| e | `conversations capability` — **simple local CLI** | 40 | 38 | 49 | 77 | 77 | 0 |
| c | `conversations search "database migration" --offline` (hit, limit-bound) | 40 | 169 | 1321 | 7317 | 7317 | 0 |
| c | `conversations search "zzzqqq-nomatch" --offline` (miss ⇒ full 10k-event scan) | 40 | 185 | 572 | 809 | 809 | 0 |
| + | `conversations sessions --offline` | 40 | 49 | 148 | 219 | 219 | 0 |
| + | `conversations history --session-id claude:sess-10148 --limit 200 --offline` | 40 | 143 | 434 | 553 | 553 | 0 |
| b | `usage usage --offline --provider opencode` (terminates; see defect D1) | 30 | 235 | 1610 | 1643 | 1643 | 0 |
| b | `usage usage --offline` on **small** fixture (3 sessions) | 30 | 393 | 1270 | 1491 | 1491 | 0 |
| b | `usage usage --offline` on **full** fixture | 1 | — | — | — | >30 000 (timeout) | 1 |
| d | `git status w2:p1` (5k-file repo) | 40 | 364 | 467 | 527 | 527 | 0 |
| d | `git diff w2:p1 --stat` | 40 | 420 | 670 | 749 | 749 | 0 |
| + | `git files w2:p1` | 40 | 488 | 808 | 1336 | 1336 | 0 |
| a | `inbox list` (16 panes) | 40 | 276 | 355 | 483 | 483 | 0 |

RSS (top-level process max, `/usr/bin/time -l`, n=12):

| Operation | RSS p50 (KB) |
| --- | --- |
| `conversations capability` | 20 640 (~20 MB) |
| `conversations search` (miss) | 39 648 (~39 MB) |
| `git status` | 27 568 (~27 MB) |
| `usage usage --provider opencode` | 35 824 (~35 MB; excludes 2 child CLIs) |
| `inbox list` | 24 096 (~24 MB) |

## Results — TUI operations (PTY probe)

`first_byte` ≈ first frame; `data_usable` ≈ first rendered content
matching the post-load marker. Raw JSON + per-run logs in
`docs/research/native-performance-baseline/`.

| Operation | n | first_byte p50/p95 | data_usable p50/p95 | pgid peak RSS p50 |
| --- | --- | --- | --- | --- |
| **(a) Inbox TUI** `bun run plugins/inbox/src/tui.ts` (marker `[0-9]+ inbox`) | 12 | 50 / 112 ms | **344 / 524 ms** | ~48 MB |
| usage TUI report, small index (`advance`→`totals:`) | 12 | 111 / 251 ms | **585 / 1387 ms** | ~71 MB |
| usage TUI report, full 10k-event index | 2 | 73 / 215 ms | **never** (>110 s, still `loading…`) | **~210–309 MB and climbing** |
| **(+) Conversations TUI** `…/tui.ts main` (marker `[0-9]+ events`, see D2) | 10 | 55 / 142 ms | n/a without keypress; ~1.54 s with forced redraw | ~49 MB |

Note: an earlier exploratory pass on the same fixtures under heavier
machine load measured Inbox data_usable p50 ≈ 666 ms / p95 ≈ 1162 ms —
the canonical (quieter) numbers above are the reference; the spread is
the shared-machine noise band referenced in the non-regression
criteria. The `usage` big-index pgid RSS reaching ~300 MB while stuck
in the D1 paging loop is direct evidence of the unbounded `events[]`
accumulation.

## Defect findings (evidence for follow-up tickets — nothing closed here)

### D1 — `conversations` multi-session paging cursor drops finished offsets ⇒ duplicate re-scan, non-terminating `usage` export

`scanPagedSessions` (`plugins/conversations/src/read.ts:211`) records
cursor offsets only for files that did **not** finish
(`page.has_more`). Finished session keys are absent from
`next_cursor`; `runPagedRead` (`plugins/conversations/src/cli.ts:852`)
then rebuilds targets with `offset: decoded[key] ?? 0`, so every
subsequent page **restarts finished sessions at byte 0**, re-emits
their matches, and — because re-emitted matches refill the page limit
before later sessions are reached — `has_more` can stay true
indefinitely once the finished prefix holds ≥ `limit` matches.

Measured evidence (fixture above, `--limit 500`):

- `conversations usage --limit 500 --offline` page 1: 500 results,
  `has_more: true`, 18 offset entries.
- Page 2 with `next_cursor`: **468 of 500 results are duplicates** of
  page 1 (`sess-10148-t1-e1-am` etc. re-emitted), `has_more: true`.
- `usage usage --offline` (unfiltered): still running after **>7 min**
  wall when killed; bounded re-run did not finish in 30 s
  (`09-usage-full-hang.json`, `failures: 1`). Root cause chain:
  `exportUsageEvents` (`plugins/usage/src/source.ts:298`) loops while
  `has_more`, up to `MAX_EXPORT_PAGES = 4000` child spawns — each page
  re-parses finished files, so this is effectively non-terminating
  (and would OOM on the accumulated `events[]` long before 4000).
- Existing tests (`tests/conversations-t3.test.ts`) exercise paging
  with a **single** session only — the multi-session offset drop is
  untested.
- Same mechanics affect `search`/`history`/`show` paging whenever
  matches exceed the page limit across multiple sessions (search page
  2 showed 36/50 duplicates). Queries with zero matches terminate
  normally (full scan, no limit hit).
- `usage --provider opencode` terminates because the selected set's
  usage events (~330) fit in one 500-event page.

Impact: **Usage aggregation is effectively broken at real-data scale**
(25 sessions / 10k events) — this is the dominant finding for Wave 1.
#294 must not build on `exportUsageEvents` paging as-is; #293 owns the
cursor/index fix. Suggested directions (for the owning tickets, not
implemented here): carry EOF offsets for finished sessions in the
cursor, or replace per-page child CLI paging with a single streaming
export / real index.

### D2 — Conversations TUI sessions view: stale empty frame until keypress (same class as #273 D-B)

`SessionsView.boot()` → `applySessions()` → `list.setItems()` never
calls `app.invalidate()` (`plugins/conversations/src/tui.ts:544-586`;
`invalidate()` exists only in `EventListView.refresh`, the #273 fix
site). Observed live: the pane renders
`(no indexed sessions — press i to index …)` **after** the CLI children
already returned 25 sessions; the list appears only after any key
press forces a redraw (verified manually and via probe: marker
`[0-9]+ events` absent for >20 s without input, present ~8 ms after a
`j` keypress at 1.5 s). Internal data readiness is ~hundreds of ms;
user-visible readiness is unbounded. Recommend folding into #293's TUI
work or a small fix ticket.

### #267 follow-up — usage TUI report latency after the stale-frame fix

#267 measured ~1.3 s "time-to-content" on an **empty** index and could
not separate CLI latency from the stale-frame defect fixed in #273.
Post-fix, real-data measurement on this baseline:

- Small index (3 sessions/600 events): report picker first frame
  p50 111 ms; `usage` report data usable p50 **585 ms** p95 1387 ms
  (includes one `usage` CLI child ≈ 235–400 ms + render).
- Full index (25 sessions/10k events): the report **never completes** —
  the child `usage` CLI hits defect D1; the view stays on `loading…`
  past 110 s (CLI child timeout is 90 s). So "real-data latency" for
  #267 is currently *unbounded*, not merely slow — root cause is D1,
  not rendering.

Evidence: `21-tui-usage-small.json`, `23-tui-usage-big.json`,
`07-usage-opencode.json`, `08-usage-small-full.json`,
`09-usage-full-hang.json`.

## Bottleneck hypotheses per operation

| Op | Dominant cost (measured/estimated) | Hypothesis |
| --- | --- | --- |
| (e) simple CLI (`capability`) | p50 38 ms ≈ bun startup 24 ms + TS module load ~14 ms | ~100 % runtime overhead; the command does zero I/O. Rust binary should approach single-digit ms. |
| (a) Inbox TUI startup | first_byte ~50 ms p50 (bun+graph); data_usable ~344 ms p50 = TUI process + **child `cli.ts list` spawn** (~200–300 ms: second bun startup + herdr snapshot IPC + state read) + render | Double Bun process startup is the main avoidable cost; snapshot IPC is small (16 panes). Rust single-process + direct snapshot read should plausibly hit first-frame ≤50 ms and data ≤200 ms. |
| (b) Usage aggregation | 3-process chain (usage CLI → `conversations usage` pages → `conversations capability`) ≈ +200–300 ms spawn overhead on top of parent (~100 ms); aggregation itself is cheap; **paging defect D1 dominates at scale** | Even after D1, per-page child respawn re-parses the index O(pages × bytes); a single-process streaming export or real index removes both. Call-count reduction potential: 2 child spawns + N page spawns → 0–1. |
| (c) Conversations search | miss (full scan) p50 185–468 ms across runs; hit p50 169 ms but p95 tail to >1 s under load | No index: every query opens+parses all 25 event files (~5.5 MB, ~50 ms pure `JSON.parse` micro-benchmark, *estimated* share smaller than observed — chunked async reads, per-line `toString`, matcher, and result serialization add the rest). External I/O ≈ 5.5 MB read; internal CPU ≈ JSON.parse ×10k. FTS5/SQLite index is the program-sanctioned fix; expect ≥10× on miss queries. |
| (d) Git status/diff | p50 364–543 ms; each command = bun startup (~40–100 ms) + herdr snapshot IPC + **~5 sequential/parallel `git` child spawns** (rev-parse ×3, status porcelain, diff) | Spawn fan-out (plugin→git×N) + double runtime startup; `git status` on 5k files itself is tens of ms (*estimated*). Rust can keep git CLI (per #286: git mutations stay CLI) but collapse plugin+snapshot overhead; call-count reduction: snapshot once per TUI session rather than per CLI invocation. |

## Budget vs measured (Program #286 targets — unmeasured stretch goals)

| #286 budget | Baseline measured | Gap / note |
| --- | --- | --- |
| Simple local CLI p95 ≤ 15 ms | p95 49 ms (p50 38) | ~3× over; nearly all Bun+TS startup. Achievable in Rust. |
| TUI first frame ≤ 50 ms | Inbox first_byte p50 50 / p95 112 ms | p50 at target, p95 ~2× over; Bun startup + module graph. |
| key→render p95 ≤ 16 ms / p99 ≤ 33 ms | **not measured** (needs interactive key-latency harness; follow-up) | TUI render path is synchronous string building — plausibly OK in Rust, unproven. |
| Indexed search p95 ≤ 50 ms | search-miss p95 572 ms (no index today); hit p95 1321 ms (load-sensitive) | Requires the real index (#293); current full-scan cannot meet it. |
| TUI RSS ≤ 20 MB normal / ≤ 50 MB large | pgid peak ~48–72 MB (upper bound, double-counts shared pages); single Bun CLI RSS already ~20–39 MB | Bun runtime alone ≈ budget; Rust should fit comfortably, but measure per-process RSS post-migration. |

## Non-regression criteria (proposal for Wave-1 gates)

Baseline = this document's canonical dataset (same fixtures, same
machine class, n≥30 CLI / n≥10 TUI, warm page cache, fresh process).

- **Latency**: a change regresses if op p95 exceeds
  `max(baseline_p95 × 1.25, baseline_p95 + 50 ms)` on the same fixture —
  the 25 %/+50 ms band exceeds observed shared-machine noise
  (stdev ≈ 25–120 ms; p95 varies ~±30 % between quiet/load windows). A
  >2× p95 increase is an automatic block regardless of fixture.
- **Tail**: p99 > 33 ms is *not* itself a gate for CLI ops (Bun noise);
  track p99 only for the key→render budget once that harness exists.
- **Failures/timeouts**: any new nonzero exit or timeout on the fixture
  commands is a regression (the `usage` full-fixture hang is a known
  defect, tracked via D1 — it must not get *worse*, e.g. slower
  time-to-first-page).
- **RSS**: pgid peak RSS within +20 % or +10 MB of baseline.
- **Correctness guard**: fixture commands must keep emitting the same
  contract envelopes (`contract`, `command`, key payload fields);
  latency wins never justify dropping `warnings`/`coverage`/`source`
  provenance fields.
- Re-run recipe: regenerate fixtures (`gen-fixtures.ts`), restart
  `np287` session, run `measure.ts`/`tui-probe.py` commands exactly as
  in this doc; compare against the committed `*.json`.

## Wave-1 ordering implications

1. **#293 Conversations first among data ops** — defect D1 blocks Usage
   (#294) entirely at scale and the missing index blocks the search
   budget; the cursor fix is small and should land before/with the
   index work. D2 (TUI stale frame) is a one-line-class fix that can
   ride along.
2. **#294 Usage** depends on #293's export path (or must bypass it);
   quota/cost/divergence add external-I/O waits that need the
   offline/cache discipline already in ADR-0014/0015 — measure those
   separately when implemented (not covered here: network).
3. **#292 Git** is the most self-contained: spawn fan-out + runtime
   overhead only; good early Rust candidate after Inbox.
4. **#291 Inbox vertical slice** benefits most from runtime
   unification (double Bun startup → single process); TUI first-frame
   and RSS budgets are both dominated by the Bun floor, so the Rust
   spike (#288/#289) de-risks it directly.
5. Simple-CLI budget (15 ms) is the purest runtime-overhead signal —
   use `conversations capability` (or equivalent zero-I/O command) as
   the canary in CI perf checks.

## Re-run commands (verbatim)

```bash
# fixtures
bun scripts/perf/gen-fixtures.ts --out /var/tmp/np287-fixtures
bun scripts/perf/gen-fixtures.ts --out /var/tmp/np287-small --events 600 --sessions 3 --files 50 --modified 5 --untracked 3

# isolated session (real herdr binary, dedicated XDG)
mkdir -p /var/tmp/np287-herdr/{config,state,data}
env -u HERDR_SOCKET_PATH -u HERDR_SESSION -u HERDR_PANE_ID -u HERDR_PLUGIN_CONTEXT_JSON \
  XDG_CONFIG_HOME=/var/tmp/np287-herdr/config XDG_STATE_HOME=/var/tmp/np287-herdr/state \
  XDG_DATA_HOME=/var/tmp/np287-herdr/data HERDR_CONFIG_PATH=/var/tmp/np287-herdr/config/herdr.toml \
  ~/.local/share/mise/installs/herdr/0.9.5/herdr --session np287 server &
# panes: workspace create --cwd <git-repo>; pane split …; pane report-agent …

# CLI measurement (example: conversations search)
env -u HERDR_SOCKET_PATH -u HERDR_PANE_ID -u HERDR_PLUGIN_CONTEXT_JSON \
  -u HERDR_PLUGIN_STATE_DIR -u HERDR_PLUGIN_CONFIG_DIR -u HERDR_TUI_CONTEXT_JSON \
  XDG_CONFIG_HOME=/var/tmp/np287-herdr/config XDG_STATE_HOME=/var/tmp/np287-herdr/state \
  XDG_DATA_HOME=/var/tmp/np287-herdr/data HERDR_SESSION=np287 \
  bun scripts/perf/measure.ts --runs 40 --warmup 5 --label conv-search-miss -- \
  bun run plugins/conversations/src/cli.ts search "zzzqqq-nomatch" --offline

# TUI measurement (example: inbox)
env -u HERDR_SOCKET_PATH -u HERDR_PANE_ID -u HERDR_PLUGIN_CONTEXT_JSON \
  -u HERDR_PLUGIN_STATE_DIR -u HERDR_PLUGIN_CONFIG_DIR -u HERDR_TUI_CONTEXT_JSON \
  XDG_CONFIG_HOME=/var/tmp/np287-herdr/config XDG_STATE_HOME=/var/tmp/np287-herdr/state \
  XDG_DATA_HOME=/var/tmp/np287-herdr/data HERDR_SESSION=np287 \
  python3 scripts/perf/tui-probe.py --label inbox-tui --runs 12 --warmup 2 \
  --timeout 30 --settle 0.4 --marker '[0-9]+ inbox' -- \
  bun run plugins/inbox/src/tui.ts
```

The full canonical suite is `/tmp/run-matrix.sh`-equivalent; every
`*.json` in `docs/research/native-performance-baseline/` records the
exact command line inside (`command` field).

## Out of scope / not measured

- `quota`/`cost`/`divergence` network paths (external I/O — needs the
  quota-credential fixtures; flagged for #294).
- key→render latency harness (interactive input→frame timing) —
  follow-up before Wave-1 RC.
- ACP/MCP-enabled process overhead (#297 scope).
- Other 9 manifests beyond the lightweight extras above — per #286,
  detailed measurement happens at each plugin's migration ticket.
